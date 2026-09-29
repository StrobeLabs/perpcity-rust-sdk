//! High-level client for the PerpCity perpetual futures protocol.
//!
//! [`PerpClient`] wires together the transport layer, HFT infrastructure,
//! and contract bindings into a single ergonomic API. It is the primary
//! entry point for interacting with PerpCity on Arbitrum (mainnet and
//! Arbitrum Sepolia testnet).
//!
//! The client accepts any [`TxSigner`] implementation: a local
//! [`PrivateKeySigner`](alloy::signers::local::PrivateKeySigner) as shown
//! below, or a remote signer such as AWS KMS via alloy's `AwsSigner`
//! (enable this crate's `aws` feature; see `examples/aws_kms_signer.rs`).
//!
//! # Example
//!
//! ```rust,no_run
//! use perpcity_sdk::{ChainReader, HftTransport, PerpClient, TransportConfig};
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
//! // One chain reader per process; every client and reader shares it.
//! let chain = ChainReader::arbitrum(transport);
//! let perp = address!("0000000000000000000000000000000000000001"); // the market's Perp contract
//!
//! let signer: PrivateKeySigner = "your_private_key_hex".parse().unwrap();
//! let client = PerpClient::new(chain.market(perp), signer);
//! # Ok(())
//! # }
//! ```

mod chain;
mod maker_equity;
mod market;
#[cfg(test)]
mod mock;
mod queries;
mod trades;
mod transactions;

pub use chain::ChainReader;
pub use maker_equity::{MAX_MAKER_EQUITY_BATCH, MakerEquityKind, MakerEquityOutcome};
pub use market::MarketReader;
pub use transactions::TxBuilder;

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::network::{Ethereum, EthereumWallet, TxSigner};
use alloy::primitives::{Address, Signature, U256, address};
use alloy::providers::{Provider, RootProvider};

use crate::constants::SCALE_1E6;
use crate::errors::Result;
use crate::hft::gas::GasLimitCache;
use crate::hft::pipeline::{PipelineConfig, TxPipeline};
use crate::hft::state_cache::{CachedBounds, CachedFees};
use crate::history::History;
use crate::transport::provider::HftTransport;
use crate::types::{Bounds, Fees};

// ── Network constants ──────────────────────────────────────────────────

/// Arbitrum One (mainnet) chain ID.
pub const ARBITRUM_CHAIN_ID: u64 = 42161;

/// Arbitrum Sepolia (testnet) chain ID.
pub const ARBITRUM_SEPOLIA_CHAIN_ID: u64 = 421614;

/// Canonical Circle USDC on Arbitrum One.
pub const ARBITRUM_USDC: Address = address!("af88d065e77c8cC2239327C5EDb3A432268e5831");

/// Collateral token used by the Arbitrum Sepolia deployment.
///
/// This is the test USDC the deployed contracts actually settle in
/// (`ExternalAddresses.sol`), NOT Circle's testnet USDC (`0x75faf…`). It is a
/// Solady-style ERC20 with an open `mint`.
pub const ARBITRUM_SEPOLIA_USDC: Address = address!("BEF280BefeE2Cb28c20D1E4Cc1da999B4DA0f1fD");

/// `PerpFactory` on Arbitrum Sepolia (the deployment markets are created from).
pub const ARBITRUM_SEPOLIA_PERP_FACTORY: Address =
    address!("a54F81e7BD5C0d52d6fdE2ba40d0B1123d53E7a7");

/// Uniswap V4 `PoolManager` on Arbitrum One.
///
/// Matches the address compiled into the Perp contracts' `Constants.sol` for
/// mainnet builds.
pub const ARBITRUM_POOL_MANAGER: Address = address!("360E68faCcca8cA495c1B759Fd9EEe466db9FB32");

/// Uniswap V4 `PoolManager` on Arbitrum Sepolia.
///
/// Matches the address compiled into the deployed Perp contracts
/// (`Constants.sol` @ `perpcity-contracts@4bbe554f`).
pub const ARBITRUM_SEPOLIA_POOL_MANAGER: Address =
    address!("FB3e0C6F74eB1a21CC1Da29aeC80D2Dfe6C9a317");

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

/// High-level client for the PerpCity protocol: one signer on one market.
///
/// A client is a [`ChainReader`] — provider, transport, history, the
/// base-fee and state caches, shared by every client over it — plus a
/// signer, the market it trades, and the transaction pipeline. All write
/// operations go through the [`TxPipeline`] for zero-RPC-on-hot-path
/// nonce/gas resolution; reads go through the reader's caches.
pub struct PerpClient {
    /// Everything the chain's readers share: provider, transport, history,
    /// the base-fee and state caches.
    chain: ChainReader,
    /// The market this client trades, read through `chain`.
    market: MarketReader,
    /// Wallet for signing transactions.
    wallet: EthereumWallet,
    /// The signer's address.
    address: Address,
    /// Transaction pipeline (nonce + gas). Mutex for interior mutability.
    pipeline: Mutex<TxPipeline>,
    /// Cached gas estimates from `eth_estimateGas`, keyed by function selector.
    gas_limit_cache: Mutex<GasLimitCache>,
}

impl std::fmt::Debug for PerpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PerpClient")
            .field("address", &self.address)
            .field("market", &self.market)
            .finish_non_exhaustive()
    }
}

/// A read helper bounded on `impl AsRef<MarketReader>` or
/// `impl AsRef<ChainReader>` takes a client where it takes a reader, so a
/// `&PerpClient` argument keeps compiling when the helper narrows to the
/// reads it makes. Use the bound, not `as_ref()` bare: with two targets,
/// the bare call is ambiguous.
impl AsRef<MarketReader> for PerpClient {
    fn as_ref(&self) -> &MarketReader {
        &self.market
    }
}

impl AsRef<ChainReader> for PerpClient {
    fn as_ref(&self) -> &ChainReader {
        &self.chain
    }
}

impl PerpClient {
    /// A signing client for `market`.
    ///
    /// `signer` is any transaction signer — a local
    /// [`PrivateKeySigner`](alloy::signers::local::PrivateKeySigner), an
    /// AWS KMS [`AwsSigner`](https://docs.rs/alloy-signer-aws) (enable the
    /// `aws` feature), or any other [`TxSigner`]. The market reader comes
    /// from [`ChainReader::market`]; build one chain reader per process and
    /// every client and reader over it shares its caches.
    ///
    /// Makes no network calls. Call [`ChainReader::refresh_gas`] and
    /// [`Self::sync_nonce`] before submitting transactions.
    pub fn new<S>(market: MarketReader, signer: S) -> Self
    where
        S: TxSigner<Signature> + Send + Sync + 'static,
    {
        let address = TxSigner::address(&signer);
        Self {
            chain: market.chain().clone(),
            market,
            wallet: EthereumWallet::from(signer),
            address,
            // Pipeline starts at nonce 0; call sync_nonce() before first tx
            pipeline: Mutex::new(TxPipeline::new(0, PipelineConfig::default())),
            gas_limit_cache: Mutex::new(GasLimitCache::new()),
        }
    }

    // ── Initialization ───────────────────────────────────────────────

    /// Sync the nonce manager with the on-chain transaction count.
    ///
    /// Must be called before the first transaction. After this, the
    /// pipeline manages nonces locally (zero RPC per transaction).
    pub async fn sync_nonce(&self) -> Result<()> {
        let count = self
            .chain
            .provider()
            .get_transaction_count(self.address)
            .await?;
        let mut pipeline = self.pipeline.lock().unwrap();
        *pipeline = TxPipeline::new(count, PipelineConfig::default());
        tracing::debug!(nonce = count, address = %self.address, "nonce synced");
        Ok(())
    }

    // ── Chain-scoped gas and caches, on the chain reader ─────────────

    /// [`ChainReader::refresh_gas`] on this client's chain reader.
    pub async fn refresh_gas(&self) -> Result<()> {
        self.chain.refresh_gas().await
    }

    /// [`ChainReader::set_base_fee`] on this client's chain reader.
    pub fn set_base_fee(&self, base_fee: u64) {
        self.chain.set_base_fee(base_fee);
    }

    /// [`ChainReader::base_fee`] on this client's chain reader.
    pub fn base_fee(&self) -> Option<u64> {
        self.chain.base_fee()
    }

    /// [`ChainReader::set_gas_ttl`] on this client's chain reader.
    pub fn set_gas_ttl(&self, ttl_ms: u64) {
        self.chain.set_gas_ttl(ttl_ms);
    }

    /// [`ChainReader::invalidate_fast_cache`] on this client's chain reader.
    pub fn invalidate_fast_cache(&self) {
        self.chain.invalidate_fast_cache();
    }

    /// [`ChainReader::invalidate_all_cache`] on this client's chain reader.
    pub fn invalidate_all_cache(&self) {
        self.chain.invalidate_all_cache();
    }

    // ── Accessors ────────────────────────────────────────────────────

    /// The signer's Ethereum address.
    pub fn address(&self) -> Address {
        self.address
    }

    /// The chain reader this client is built over.
    pub fn chain(&self) -> &ChainReader {
        &self.chain
    }

    /// The reader for the market this client trades.
    pub fn market(&self) -> &MarketReader {
        &self.market
    }

    /// [`ChainReader::provider`] on this client's chain reader.
    pub fn provider(&self) -> &RootProvider<Ethereum> {
        self.chain.provider()
    }

    /// [`ChainReader::history`] on this client's chain reader.
    pub fn history(&self) -> &History<RootProvider<Ethereum>> {
        self.chain.history()
    }

    /// The signing wallet (for building signed transactions outside the SDK).
    pub fn wallet(&self) -> &EthereumWallet {
        &self.wallet
    }

    /// [`ChainReader::transport`] on this client's chain reader.
    pub fn transport(&self) -> &HftTransport {
        self.chain.transport()
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
    use alloy::primitives::{B256, Bytes};
    use alloy::rpc::types::{Filter, Log};

    use super::*;
    use crate::client::mock;
    use crate::history::test_support::{FakeNode, mined_log};

    // ── The history handle ───────────────────────────────────────────

    const EMITTER: Address = Address::repeat_byte(0xe1);
    const TOPIC: B256 = B256::repeat_byte(0x70);

    fn logs_every(step: u64, last: u64) -> Vec<Log> {
        (0..=last)
            .step_by(step as usize)
            .map(|block| mined_log(EMITTER, TOPIC, Bytes::new(), block, 0))
            .collect()
    }

    /// The client keeps one handle: what one scan learns about the
    /// provider's `eth_getLogs` width, the next scan through the accessor
    /// starts from, and the counters run across both.
    #[tokio::test]
    async fn history_is_one_handle_for_the_clients_lifetime() {
        let cap = 10_000;
        let node = FakeNode::new(logs_every(777, 120_000), cap);
        let client = mock::client_over(node.provider());
        let filter = Filter::new().address(EMITTER).event_signature(TOPIC);

        client
            .history()
            .logs(&filter, 0, Some(120_000))
            .await
            .unwrap();
        let first = client.history().stats();
        let after_first = node.requests().len();
        assert!(first.requests > 0);

        client
            .history()
            .logs(&filter, 120_001, Some(240_000))
            .await
            .unwrap();
        let second = client.history().stats();
        assert!(
            second.requests > first.requests,
            "counters accumulate across calls to the accessor"
        );
        let (from, to) = node.requests()[after_first];
        assert!(
            to - from < cap,
            "the second scan started at the learned width, not the full span"
        );
    }

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
