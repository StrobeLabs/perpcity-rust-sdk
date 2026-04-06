//! High-level client for the PerpCity perpetual futures protocol.
//!
//! [`PerpClient`] wires together the transport layer, HFT infrastructure,
//! and contract bindings into a single ergonomic API. It is the primary
//! entry point for interacting with PerpCity on Base L2.
//!
//! # Example
//!
//! ```rust,no_run
//! use perpcity_sdk::{PerpClient, Deployments, HftTransport, TransportConfig};
//! use alloy::primitives::{address, Address, B256};
//! use alloy::signers::local::PrivateKeySigner;
//!
//! # async fn example() -> perpcity_sdk::Result<()> {
//! let transport = HftTransport::new(
//!     TransportConfig::builder()
//!         .shared_endpoint("https://mainnet.base.org")
//!         .build()?
//! )?;
//!
//! let signer: PrivateKeySigner = "your_private_key_hex".parse().unwrap();
//!
//! let deployments = Deployments {
//!     perp_manager: address!("0000000000000000000000000000000000000001"),
//!     usdc: address!("C1a5D4E99BB224713dd179eA9CA2Fa6600706210"),
//!     fees_module: None,
//!     margin_ratios_module: None,
//!     lockup_period_module: None,
//!     sqrt_price_impact_limit_module: None,
//! };
//!
//! let client = PerpClient::new(transport, signer, deployments, 8453)?;
//! # Ok(())
//! # }
//! ```

mod queries;
mod trades;
mod transactions;

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::network::{Ethereum, EthereumWallet};
use alloy::primitives::{Address, I256, U256};
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::client::RpcClient;
use alloy::signers::local::PrivateKeySigner;
use alloy::transports::BoxTransport;

use crate::constants::SCALE_1E6;
use crate::contracts::PerpManager;
use crate::convert::scale_from_6dec;
use crate::errors::{ContractError, Result, TransactionError};
use crate::hft::gas::{FeeCache, GasLimitCache};
use crate::hft::pipeline::{PipelineConfig, TxPipeline};
use crate::hft::state_cache::{CachedBounds, CachedFees, StateCache, StateCacheConfig};
use crate::transport::provider::HftTransport;
use crate::types::{
    AdjustMarginResult, AdjustNotionalResult, Bounds, CloseResult, Deployments, Fees, OpenResult,
};

// ── Constants ────────────────────────────────────────────────────────

/// Base L2 chain ID.
const BASE_CHAIN_ID: u64 = 8453;

/// Default gas cache TTL: 2 seconds (2 Base L2 blocks).
const DEFAULT_GAS_TTL_MS: u64 = 2_000;

/// Default priority fee: 0.01 gwei.
///
/// Base L2 uses a single sequencer, so priority fees are near-meaningless.
/// 10 Mwei is sufficient for reliable inclusion while keeping gas escrow low.
const DEFAULT_PRIORITY_FEE: u64 = 10_000_000;

/// Maximum USDC approval amount (2^256 - 1).
const MAX_APPROVAL: U256 = U256::MAX;

/// SCALE_1E6 as f64, used for converting on-chain fixed-point values.
const SCALE_F64: f64 = SCALE_1E6 as f64;

/// Convert a Q96 fixed-point funding-per-second value to a daily rate.
fn funding_x96_to_daily(funding_x96: I256) -> f64 {
    let funding_i128 = i128_from_i256(funding_x96);
    let rate_per_sec = funding_i128 as f64 / 2.0_f64.powi(96);
    rate_per_sec * crate::constants::INTERVAL as f64
}

// ── From impls for cache ↔ client type bridging ────────────────────────

impl From<CachedFees> for Fees {
    fn from(c: CachedFees) -> Self {
        Self {
            creator_fee: c.creator_fee,
            insurance_fee: c.insurance_fee,
            lp_fee: c.lp_fee,
            liquidation_fee: c.liquidation_fee,
        }
    }
}

impl From<Fees> for CachedFees {
    fn from(f: Fees) -> Self {
        Self {
            creator_fee: f.creator_fee,
            insurance_fee: f.insurance_fee,
            lp_fee: f.lp_fee,
            liquidation_fee: f.liquidation_fee,
        }
    }
}

impl From<CachedBounds> for Bounds {
    fn from(c: CachedBounds) -> Self {
        Self {
            min_margin: c.min_margin,
            min_taker_leverage: c.min_taker_leverage,
            max_taker_leverage: c.max_taker_leverage,
            liquidation_taker_ratio: c.liquidation_taker_ratio,
        }
    }
}

impl From<Bounds> for CachedBounds {
    fn from(b: Bounds) -> Self {
        Self {
            min_margin: b.min_margin,
            min_taker_leverage: b.min_taker_leverage,
            max_taker_leverage: b.max_taker_leverage,
            liquidation_taker_ratio: b.liquidation_taker_ratio,
        }
    }
}

// ── PerpClient ───────────────────────────────────────────────────────

/// High-level client for the PerpCity protocol.
///
/// Combines transport, signing, transaction pipeline, state caching, and
/// contract bindings into one ergonomic API. All write operations go
/// through the [`TxPipeline`] for zero-RPC-on-hot-path nonce/gas resolution.
/// Read operations use the [`StateCache`] to avoid redundant RPC calls.
pub struct PerpClient {
    /// Alloy provider wired to HftTransport (multi-endpoint, health-aware).
    provider: RootProvider<Ethereum>,
    /// The underlying transport (kept for health diagnostics).
    transport: HftTransport,
    /// Wallet for signing transactions.
    wallet: EthereumWallet,
    /// The signer's address.
    address: Address,
    /// Deployed contract addresses.
    deployments: Deployments,
    /// Chain ID for transaction building.
    chain_id: u64,
    /// Transaction pipeline (nonce + gas). Mutex for interior mutability.
    pipeline: Mutex<TxPipeline>,
    /// Gas fee cache, updated from block headers.
    fee_cache: Mutex<FeeCache>,
    /// Cached gas estimates from `eth_estimateGas`, keyed by function selector.
    gas_limit_cache: Mutex<GasLimitCache>,
    /// Multi-layer state cache for on-chain reads.
    state_cache: Mutex<StateCache>,
}

impl std::fmt::Debug for PerpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PerpClient")
            .field("address", &self.address)
            .field("chain_id", &self.chain_id)
            .field("deployments", &self.deployments)
            .finish_non_exhaustive()
    }
}

impl PerpClient {
    /// Create a new PerpClient.
    ///
    /// - `transport`: Multi-endpoint RPC transport (from [`crate::TransportConfig`])
    /// - `signer`: Private key for signing transactions
    /// - `deployments`: Contract addresses for this PerpCity instance
    /// - `chain_id`: Chain ID (8453 for Base mainnet, 84532 for Base Sepolia)
    ///
    /// This does NOT make any network calls. Call [`Self::refresh_gas`] and
    /// [`Self::sync_nonce`] before submitting transactions.
    pub fn new(
        transport: HftTransport,
        signer: PrivateKeySigner,
        deployments: Deployments,
        chain_id: u64,
    ) -> Result<Self> {
        let address = signer.address();
        let wallet = EthereumWallet::from(signer);

        let boxed = BoxTransport::new(transport.clone());
        let rpc_client = RpcClient::new(boxed, false);
        let provider = RootProvider::<Ethereum>::new(rpc_client);

        Ok(Self {
            provider,
            transport,
            wallet,
            address,
            deployments,
            chain_id,
            // Pipeline starts at nonce 0; call sync_nonce() before first tx
            pipeline: Mutex::new(TxPipeline::new(0, PipelineConfig::default())),
            fee_cache: Mutex::new(FeeCache::new(DEFAULT_GAS_TTL_MS, DEFAULT_PRIORITY_FEE)),
            gas_limit_cache: Mutex::new(GasLimitCache::new()),
            state_cache: Mutex::new(StateCache::new(StateCacheConfig::default())),
        })
    }

    /// Create a client pre-configured for Base mainnet.
    pub fn new_base_mainnet(
        transport: HftTransport,
        signer: PrivateKeySigner,
        deployments: Deployments,
    ) -> Result<Self> {
        Self::new(transport, signer, deployments, BASE_CHAIN_ID)
    }

    // ── Initialization ───────────────────────────────────────────────

    /// Sync the nonce manager with the on-chain transaction count.
    ///
    /// Must be called before the first transaction. After this, the
    /// pipeline manages nonces locally (zero RPC per transaction).
    pub async fn sync_nonce(&self) -> Result<()> {
        let count = self.provider.get_transaction_count(self.address).await?;
        let mut pipeline = self.pipeline.lock().unwrap();
        *pipeline = TxPipeline::new(count, PipelineConfig::default());
        tracing::info!(nonce = count, address = %self.address, "nonce synced");
        Ok(())
    }

    /// Refresh the gas cache from the latest block header.
    ///
    /// Fetches the latest block directly in a single RPC call and extracts
    /// the base fee for EIP-1559 fee computation. Should be called
    /// periodically (every 1-2 seconds on Base L2) or from a `newHeads`
    /// subscription callback.
    pub async fn refresh_gas(&self) -> Result<()> {
        let header = self
            .provider
            .get_block_by_number(alloy::eips::BlockNumberOrTag::Latest)
            .await?
            .ok_or_else(|| TransactionError::GasUnavailable {
                reason: "latest block not found".into(),
            })?;

        let base_fee =
            header
                .header
                .base_fee_per_gas
                .ok_or_else(|| TransactionError::GasUnavailable {
                    reason: "block has no base fee (pre-EIP-1559?)".into(),
                })?;

        let now = now_ms();
        self.fee_cache.lock().unwrap().update(base_fee, now);
        tracing::debug!(base_fee, "gas cache refreshed");
        Ok(())
    }

    /// Inject a base fee from an external source (e.g. a shared poller).
    ///
    /// Updates the gas cache as if `refresh_gas` had been called, but without
    /// any RPC calls. The cache TTL is reset to now.
    pub fn set_base_fee(&self, base_fee: u64) {
        let now = now_ms();
        self.fee_cache.lock().unwrap().update(base_fee, now);
        tracing::debug!(base_fee, "base fee injected");
    }

    /// Return the current cached base fee, if any (ignores TTL).
    ///
    /// Intended for reading the base fee after `refresh_gas` in order to
    /// distribute it to other clients via [`set_base_fee`](Self::set_base_fee).
    pub fn base_fee(&self) -> Option<u64> {
        self.fee_cache.lock().unwrap().base_fee()
    }

    /// Override the gas cache TTL (milliseconds).
    ///
    /// When gas is managed externally via [`set_base_fee`](Self::set_base_fee),
    /// the default 2s TTL may be too tight. Set this to match the poller's
    /// cadence with headroom (e.g. `tick_secs * 2 * 1000`).
    pub fn set_gas_ttl(&self, ttl_ms: u64) {
        self.fee_cache.lock().unwrap().set_ttl(ttl_ms);
        tracing::debug!(ttl_ms, "gas cache TTL updated");
    }

    // ── Accessors ────────────────────────────────────────────────────

    /// The signer's Ethereum address.
    pub fn address(&self) -> Address {
        self.address
    }

    /// The deployed contract addresses.
    pub fn deployments(&self) -> &Deployments {
        &self.deployments
    }

    /// The underlying Alloy provider (for advanced queries).
    pub fn provider(&self) -> &RootProvider<Ethereum> {
        &self.provider
    }

    /// The signing wallet (for building signed transactions outside the SDK).
    pub fn wallet(&self) -> &EthereumWallet {
        &self.wallet
    }

    /// The underlying HFT transport (for health diagnostics).
    pub fn transport(&self) -> &HftTransport {
        &self.transport
    }

    /// Invalidate the fast cache layer (prices, funding, balance).
    ///
    /// Call on new-block events to ensure fresh data.
    pub fn invalidate_fast_cache(&self) {
        let mut cache = self.state_cache.lock().unwrap();
        cache.invalidate_fast_layer();
    }

    /// Invalidate all cached state.
    pub fn invalidate_all_cache(&self) {
        let mut cache = self.state_cache.lock().unwrap();
        cache.invalidate_all();
    }

    /// Resolve a transaction (mined, reverted, or timed out).
    /// Removes from in-flight tracking without rewinding the nonce.
    pub fn resolve_tx(&self, tx_hash: &[u8; 32]) {
        let mut pipeline = self.pipeline.lock().unwrap();
        pipeline.resolve(tx_hash);
    }

    /// Mark a transaction as failed. Releases the nonce if possible.
    pub fn fail_tx(&self, tx_hash: &[u8; 32]) {
        let mut pipeline = self.pipeline.lock().unwrap();
        pipeline.fail(tx_hash);
    }

    /// Number of currently in-flight (unconfirmed) transactions.
    pub fn in_flight_count(&self) -> usize {
        let pipeline = self.pipeline.lock().unwrap();
        pipeline.in_flight_count()
    }
}

// ── Type conversion helpers for Alloy fixed-size types ───────────────

/// Convert a u32 margin ratio to Alloy's uint24 type.
#[inline]
fn u32_to_u24(v: u32) -> alloy::primitives::Uint<24, 1> {
    alloy::primitives::Uint::<24, 1>::from(v & 0xFF_FFFF)
}

/// Convert Alloy's uint24 to a u32.
#[inline]
fn u24_to_u32(v: alloy::primitives::Uint<24, 1>) -> u32 {
    v.to::<u32>()
}

/// Convert an i32 tick to Alloy's int24 type.
#[inline]
fn i32_to_i24(v: i32) -> alloy::primitives::Signed<24, 1> {
    alloy::primitives::Signed::<24, 1>::try_from(v as i64).unwrap_or(if v < 0 {
        alloy::primitives::Signed::<24, 1>::MIN
    } else {
        alloy::primitives::Signed::<24, 1>::MAX
    })
}

/// Convert Alloy's int24 to an i32.
#[inline]
fn i24_to_i32(v: alloy::primitives::Signed<24, 1>) -> i32 {
    // int24 always fits in i32
    v.as_i32()
}

// ── Utility functions ────────────────────────────────────────────────

/// Get current time in milliseconds.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Get current time in seconds (for state cache).
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Convert an I256 to i128 (clamping to i128::MIN/MAX on overflow).
#[inline]
fn i128_from_i256(v: I256) -> i128 {
    i128::try_from(v).unwrap_or_else(|_| {
        if v.is_negative() {
            i128::MIN
        } else {
            i128::MAX
        }
    })
}

/// Scale an unsigned `U256` from 6-decimal on-chain representation to `f64`.
fn u256_to_f64_6dec(v: U256) -> f64 {
    v.to::<u128>() as f64 / 1_000_000.0
}

/// Parse an [`OpenResult`] from a transaction receipt's `PositionOpened` event.
fn parse_open_result(
    receipt: &alloy::rpc::types::TransactionReceipt,
) -> std::result::Result<OpenResult, ContractError> {
    for log in receipt.inner.logs() {
        if let Ok(event) = log.log_decode::<PerpManager::PositionOpened>() {
            let data = event.inner.data;
            let perp_delta = i128_from_i256(data.perpDelta);
            let usd_delta = i128_from_i256(data.usdDelta);
            return Ok(OpenResult {
                pos_id: data.posId,
                is_maker: data.isMaker,
                perp_delta: scale_from_6dec(perp_delta),
                usd_delta: scale_from_6dec(usd_delta),
                tick_lower: i24_to_i32(data.tickLower),
                tick_upper: i24_to_i32(data.tickUpper),
            });
        }
    }
    Err(ContractError::EventNotFound {
        event_name: "PositionOpened".into(),
    })
}

/// Parse an [`AdjustNotionalResult`] from a transaction receipt's `NotionalAdjusted` event.
fn parse_adjust_result(
    receipt: &alloy::rpc::types::TransactionReceipt,
) -> std::result::Result<AdjustNotionalResult, ContractError> {
    for log in receipt.inner.logs() {
        if let Ok(event) = log.log_decode::<PerpManager::NotionalAdjusted>() {
            let data = event.inner.data;
            return Ok(AdjustNotionalResult {
                new_perp_delta: scale_from_6dec(i128_from_i256(data.newPerpDelta)),
                swap_perp_delta: scale_from_6dec(i128_from_i256(data.swapPerpDelta)),
                swap_usd_delta: scale_from_6dec(i128_from_i256(data.swapUsdDelta)),
                funding: scale_from_6dec(i128_from_i256(data.funding)),
                utilization_fee: u256_to_f64_6dec(data.utilizationFee),
                adl: u256_to_f64_6dec(data.adl),
                trading_fees: u256_to_f64_6dec(data.tradingFees),
            });
        }
    }
    Err(ContractError::EventNotFound {
        event_name: "NotionalAdjusted".into(),
    })
}

/// Parse an [`AdjustMarginResult`] from a transaction receipt's `MarginAdjusted` event.
fn parse_margin_result(
    receipt: &alloy::rpc::types::TransactionReceipt,
) -> std::result::Result<AdjustMarginResult, ContractError> {
    for log in receipt.inner.logs() {
        if let Ok(event) = log.log_decode::<PerpManager::MarginAdjusted>() {
            return Ok(AdjustMarginResult {
                new_margin: u256_to_f64_6dec(event.inner.data.newMargin),
            });
        }
    }
    Err(ContractError::EventNotFound {
        event_name: "MarginAdjusted".into(),
    })
}

/// Parse a [`CloseResult`] from a transaction receipt's `PositionClosed` event.
fn parse_close_result(
    receipt: &alloy::rpc::types::TransactionReceipt,
    pos_id: U256,
) -> std::result::Result<CloseResult, ContractError> {
    let tx_hash = receipt.transaction_hash;
    for log in receipt.inner.logs() {
        if let Ok(event) = log.log_decode::<PerpManager::PositionClosed>() {
            let data = event.inner.data;
            return Ok(CloseResult {
                tx_hash,
                was_maker: data.wasMaker,
                was_liquidated: data.wasLiquidated,
                remaining_position_id: if data.wasPartialClose {
                    Some(pos_id)
                } else {
                    None
                },
                exit_perp_delta: scale_from_6dec(i128_from_i256(data.exitPerpDelta)),
                exit_usd_delta: scale_from_6dec(i128_from_i256(data.exitUsdDelta)),
                net_usd_delta: scale_from_6dec(i128_from_i256(data.netUsdDelta)),
                funding: scale_from_6dec(i128_from_i256(data.funding)),
                utilization_fee: u256_to_f64_6dec(data.utilizationFee),
                adl: u256_to_f64_6dec(data.adl),
                liquidation_fee: u256_to_f64_6dec(data.liquidationFee),
                net_margin: scale_from_6dec(i128_from_i256(data.netMargin)),
            });
        }
    }
    Err(ContractError::EventNotFound {
        event_name: "PositionClosed".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── i128_from_i256 tests ─────────────────────────────────────────

    #[test]
    fn i128_from_i256_small_values() {
        assert_eq!(i128_from_i256(I256::ZERO), 0);
        assert_eq!(i128_from_i256(I256::try_from(42i64).unwrap()), 42);
        assert_eq!(i128_from_i256(I256::try_from(-100i64).unwrap()), -100);
    }

    #[test]
    fn i128_from_i256_boundary_values() {
        let max_i128 = I256::try_from(i128::MAX).unwrap();
        assert_eq!(i128_from_i256(max_i128), i128::MAX);

        let min_i128 = I256::try_from(i128::MIN).unwrap();
        assert_eq!(i128_from_i256(min_i128), i128::MIN);
    }

    #[test]
    fn i128_from_i256_overflow_clamps() {
        assert_eq!(i128_from_i256(I256::MAX), i128::MAX);
        assert_eq!(i128_from_i256(I256::MIN), i128::MIN);
    }

    #[test]
    fn i128_from_i256_just_beyond_i128() {
        let beyond = I256::try_from(i128::MAX).unwrap() + I256::try_from(1i64).unwrap();
        assert_eq!(i128_from_i256(beyond), i128::MAX);

        let below = I256::try_from(i128::MIN).unwrap() - I256::try_from(1i64).unwrap();
        assert_eq!(i128_from_i256(below), i128::MIN);
    }

    // ── Type conversion helpers ──────────────────────────────────────

    #[test]
    fn u24_roundtrip() {
        for v in [0u32, 1, 100_000, 0xFF_FFFF] {
            let u24 = u32_to_u24(v);
            assert_eq!(u24_to_u32(u24), v);
        }
    }

    #[test]
    fn u24_truncates_overflow() {
        // Values > 0xFFFFFF get masked
        let u24 = u32_to_u24(0x1FF_FFFF);
        assert_eq!(u24_to_u32(u24), 0xFF_FFFF);
    }

    #[test]
    fn i24_roundtrip() {
        for v in [0i32, 1, -1, 30, -30, 69_090, -69_090] {
            let i24 = i32_to_i24(v);
            assert_eq!(i24_to_i32(i24), v);
        }
    }

    // ── Funding rate integration test ───────────────────────────────

    #[test]
    fn funding_rate_x96_conversion() {
        let q96 = 2.0_f64.powi(96);
        let rate_per_sec = 0.0001;
        let x96_value = (rate_per_sec * q96) as i128;
        let i256_val = I256::try_from(x96_value).unwrap();

        let recovered = i128_from_i256(i256_val) as f64 / q96;
        let daily = recovered * 86400.0;

        assert!((recovered - rate_per_sec).abs() < 1e-10);
        assert!((daily - 8.64).abs() < 0.001);
    }
}
