//! High-level client for the PerpCity perpetual futures protocol.
//!
//! [`PerpClient`] wires together the transport layer, HFT infrastructure,
//! and contract bindings into a single ergonomic API. It is the primary
//! entry point for interacting with PerpCity on Arbitrum (mainnet and
//! Arbitrum Sepolia testnet).
//!
//! # Example
//!
//! ```rust,no_run
//! use perpcity_sdk::{PerpClient, Deployments, HftTransport, TransportConfig, ARBITRUM_USDC};
//! use alloy::primitives::address;
//! use alloy::signers::local::PrivateKeySigner;
//!
//! # async fn example() -> perpcity_sdk::Result<()> {
//! let transport = HftTransport::new(
//!     TransportConfig::builder()
//!         .shared_endpoint("https://arb1.arbitrum.io/rpc")
//!         .build()?
//! )?;
//!
//! let signer: PrivateKeySigner = "your_private_key_hex".parse().unwrap();
//!
//! let deployments = Deployments {
//!     perp: address!("0000000000000000000000000000000000000001"), // the market's Perp contract
//!     usdc: ARBITRUM_USDC,
//! };
//!
//! let client = PerpClient::new_arbitrum(transport, signer, deployments)?;
//! # Ok(())
//! # }
//! ```

mod queries;
mod trades;
mod transactions;

pub use transactions::TxBuilder;

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::network::{Ethereum, EthereumWallet};
use alloy::primitives::{Address, U256, address};
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::client::RpcClient;
use alloy::signers::local::PrivateKeySigner;
use alloy::transports::BoxTransport;

use crate::constants::SCALE_1E6;
use crate::errors::{Result, TransactionError};
use crate::hft::gas::{FeeCache, GasLimitCache};
use crate::hft::pipeline::{PipelineConfig, TxPipeline};
use crate::hft::state_cache::{CachedBounds, CachedFees, StateCache, StateCacheConfig};
use crate::transport::provider::HftTransport;
use crate::types::{Bounds, Deployments, Fees};

// ── Network constants ──────────────────────────────────────────────────

/// Arbitrum One (mainnet) chain ID.
pub const ARBITRUM_CHAIN_ID: u64 = 42161;

/// Arbitrum Sepolia (testnet) chain ID.
pub const ARBITRUM_SEPOLIA_CHAIN_ID: u64 = 421614;

/// Canonical USDC on Arbitrum One.
pub const ARBITRUM_USDC: Address = address!("af88d065e77c8cC2239327C5EDb3A432268e5831");

/// USDC on Arbitrum Sepolia (testnet).
pub const ARBITRUM_SEPOLIA_USDC: Address = address!("75faf114eafb1BDbe2F0316DF893fd58CE46AA4d");

/// Default gas cache TTL: 2 seconds.
const DEFAULT_GAS_TTL_MS: u64 = 2_000;

/// Default priority fee: 0.01 gwei.
///
/// Arbitrum sequences transactions first-come-first-served, so priority fees
/// have little effect; 10 Mwei keeps gas escrow low while remaining a valid
/// non-zero tip.
///
/// NOTE: this models only the L2 execution fee. Arbitrum also charges an L1
/// calldata (data-availability) component that is not yet accounted for here —
/// see the gas-model follow-up.
const DEFAULT_PRIORITY_FEE: u64 = 10_000_000;

/// Maximum USDC approval amount (2^256 - 1).
const MAX_APPROVAL: U256 = U256::MAX;

/// SCALE_1E6 as f64, used for converting on-chain fixed-point values.
const SCALE_F64: f64 = SCALE_1E6 as f64;

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
    /// - `chain_id`: Chain ID (42161 for Arbitrum One, 421614 for Arbitrum Sepolia)
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

    /// Create a client pre-configured for Arbitrum One (mainnet).
    pub fn new_arbitrum(
        transport: HftTransport,
        signer: PrivateKeySigner,
        deployments: Deployments,
    ) -> Result<Self> {
        Self::new(transport, signer, deployments, ARBITRUM_CHAIN_ID)
    }

    /// Create a client pre-configured for Arbitrum Sepolia (testnet).
    pub fn new_arbitrum_sepolia(
        transport: HftTransport,
        signer: PrivateKeySigner,
        deployments: Deployments,
    ) -> Result<Self> {
        Self::new(transport, signer, deployments, ARBITRUM_SEPOLIA_CHAIN_ID)
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
        tracing::debug!(nonce = count, address = %self.address, "nonce synced");
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── Type conversion helpers ──────────────────────────────────────

    #[test]
    fn u24_roundtrip() {
        for v in [0u32, 1, 100_000, 0xFF_FFFF] {
            let u24 = alloy::primitives::Uint::<24, 1>::from(v);
            assert_eq!(u24_to_u32(u24), v);
        }
    }

    #[test]
    fn i24_roundtrip() {
        for v in [0i32, 1, -1, 30, -30, 69_090, -69_090] {
            let i24 = i32_to_i24(v);
            assert_eq!(i24_to_i32(i24), v);
        }
    }

}
