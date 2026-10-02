//! The chain-scoped read handle, [`ChainReader`].

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::Ethereum;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{Empty, MulticallBuilder, Provider, RootProvider};
use alloy::rpc::client::RpcClient;
use alloy::sol_types::{SolCall, SolValue};
use alloy::transports::BoxTransport;
use serde::{Deserialize, Serialize};

use crate::constants::{MULTICALL3, SNAPSHOT_BLOCK_LAG};
use crate::contracts::{IBeacon, IERC20, IMulticall3};
use crate::convert::{price_x96_to_f64, usdc_from_atoms};
use crate::errors::{ContractError, Result, TransactionError, ValidationError};
use crate::hft::gas::FeeCache;
use crate::hft::state_cache::{BalanceKey, StateCache, StateCacheConfig};
use crate::history::History;
use crate::math::BlockContext;
use crate::transport::provider::HftTransport;

use super::queries::MarketImmutables;
use super::transactions::{classify_simulation_failure, preflight_request};
use super::{
    ARBITRUM_CHAIN_ID, ARBITRUM_POOL_MANAGER, ARBITRUM_SEPOLIA_CHAIN_ID,
    ARBITRUM_SEPOLIA_POOL_MANAGER, ARBITRUM_SEPOLIA_USDC, ARBITRUM_USDC, DEFAULT_GAS_TTL_MS,
    DEFAULT_PRIORITY_FEE, now_ms, now_secs,
};

/// The addresses every market on a chain shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainDeployments {
    /// The collateral token.
    pub usdc: Address,
    /// The Uniswap V4 `PoolManager` every market's pool lives in (see
    /// `ARBITRUM_POOL_MANAGER` / `ARBITRUM_SEPOLIA_POOL_MANAGER`).
    pub pool_manager: Address,
}

/// What every market on a chain shares, and the reads addressed by
/// something other than a market.
///
/// A reader holds the provider and its transport, the chain id, the
/// collateral token and the pool manager, one [`History`] handle, the
/// base-fee cache and the state cache, and answers a holder's balances, a
/// beacon's index, and the lagged block that pinned reads use. It needs no
/// signer. Clone is cheap (Arc), and every client built over one reader
/// shares its caches.
#[derive(Clone)]
pub struct ChainReader {
    inner: Arc<Inner>,
}

struct Inner {
    /// Alloy provider wired to the transport.
    provider: RootProvider<Ethereum>,
    /// The transport: health diagnostics, and the endpoint capabilities
    /// the reads consult.
    transport: HftTransport,
    /// Chain id, for transaction building.
    chain_id: u64,
    /// The addresses every market on the chain shares.
    deployments: ChainDeployments,
    /// One history handle for the reader's lifetime, so the learned
    /// `eth_getLogs` width and the scan counters carry across calls.
    history: History<RootProvider<Ethereum>>,
    /// Base-fee cache, updated from block headers.
    fees: Mutex<FeeCache>,
    /// TTL cache over on-chain reads, keyed per market and per holder.
    state: Mutex<StateCache>,
    /// Each market's deployment-fixed pool values, read once per market
    /// by whichever reader asks first. Immutables never go stale, so
    /// there is no TTL.
    immutables: Mutex<HashMap<Address, MarketImmutables>>,
}

impl fmt::Debug for ChainReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChainReader")
            .field("chain_id", &self.inner.chain_id)
            .field("deployments", &self.inner.deployments)
            .finish_non_exhaustive()
    }
}

/// So a helper bounded on `impl AsRef<ChainReader>` takes a reader, a
/// market reader or a client alike.
impl AsRef<ChainReader> for ChainReader {
    fn as_ref(&self) -> &ChainReader {
        self
    }
}

impl ChainReader {
    /// A reader over `transport` for the chain `deployments` describe.
    /// Makes no network calls.
    pub fn new(transport: HftTransport, deployments: ChainDeployments, chain_id: u64) -> Self {
        let rpc_client = RpcClient::new(BoxTransport::new(transport.clone()), false);
        let provider = RootProvider::<Ethereum>::new(rpc_client);
        Self::from_parts(provider, transport, deployments, chain_id)
    }

    /// A reader for Arbitrum One: canonical Circle USDC and the mainnet
    /// `PoolManager`.
    pub fn arbitrum(transport: HftTransport) -> Self {
        Self::new(
            transport,
            ChainDeployments {
                usdc: ARBITRUM_USDC,
                pool_manager: ARBITRUM_POOL_MANAGER,
            },
            ARBITRUM_CHAIN_ID,
        )
    }

    /// A reader for Arbitrum Sepolia: the deployment's test USDC and the
    /// testnet `PoolManager`.
    pub fn arbitrum_sepolia(transport: HftTransport) -> Self {
        Self::new(
            transport,
            ChainDeployments {
                usdc: ARBITRUM_SEPOLIA_USDC,
                pool_manager: ARBITRUM_SEPOLIA_POOL_MANAGER,
            },
            ARBITRUM_SEPOLIA_CHAIN_ID,
        )
    }

    /// Assemble a reader around an already-built provider. [`Self::new`]
    /// wires the provider to `transport`; the mocked reader in tests does
    /// not, which is the only reason the two are separate.
    pub(super) fn from_parts(
        provider: RootProvider<Ethereum>,
        transport: HftTransport,
        deployments: ChainDeployments,
        chain_id: u64,
    ) -> Self {
        let history = History::new(provider.clone());
        Self {
            inner: Arc::new(Inner {
                provider,
                transport,
                chain_id,
                deployments,
                history,
                fees: Mutex::new(FeeCache::new(DEFAULT_GAS_TTL_MS, DEFAULT_PRIORITY_FEE)),
                state: Mutex::new(StateCache::new(StateCacheConfig::default())),
                immutables: Mutex::new(HashMap::new()),
            }),
        }
    }

    // ── Accessors ────────────────────────────────────────────────────

    /// The underlying Alloy provider (for advanced queries).
    pub fn provider(&self) -> &RootProvider<Ethereum> {
        &self.inner.provider
    }

    /// The underlying HFT transport (for health diagnostics).
    pub fn transport(&self) -> &HftTransport {
        &self.inner.transport
    }

    /// The chain id.
    pub fn chain_id(&self) -> u64 {
        self.inner.chain_id
    }

    /// The addresses every market on the chain shares.
    pub fn deployments(&self) -> &ChainDeployments {
        &self.inner.deployments
    }

    /// The reader's [`History`] handle, with the default lag policy. It is
    /// one handle for the reader's lifetime, so a process that scans
    /// repeatedly pays the width search once and [`History::stats`] meters
    /// every scan. For another lag or concurrency budget, build a handle
    /// with [`History::new`] over [`Self::provider`].
    pub fn history(&self) -> &History<RootProvider<Ethereum>> {
        &self.inner.history
    }

    /// The base-fee cache, for the send path.
    pub(super) fn fee_cache(&self) -> &Mutex<FeeCache> {
        &self.inner.fees
    }

    /// The state cache, for the market reads.
    pub(super) fn state_cache(&self) -> &Mutex<StateCache> {
        &self.inner.state
    }

    /// The per-market immutables, for the market reads.
    pub(super) fn immutables_cache(&self) -> &Mutex<HashMap<Address, MarketImmutables>> {
        &self.inner.immutables
    }

    // ── Gas ──────────────────────────────────────────────────────────

    /// Refresh the gas cache from the latest block header.
    ///
    /// Fetches the latest block directly in a single RPC call and extracts
    /// the base fee for EIP-1559 fee computation. Should be called
    /// periodically (every 1-2 seconds) or from a `newHeads`
    /// subscription callback.
    pub async fn refresh_gas(&self) -> Result<()> {
        let header = self
            .inner
            .provider
            .get_block_by_number(BlockNumberOrTag::Latest)
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
        self.inner.fees.lock().unwrap().update(base_fee, now);
        tracing::debug!(base_fee, "gas cache refreshed");
        Ok(())
    }

    /// Inject a base fee from an external source (e.g. a block feed).
    ///
    /// Updates the gas cache as if `refresh_gas` had been called, but
    /// without any RPC calls. The cache TTL is reset to now.
    pub fn set_base_fee(&self, base_fee: u64) {
        let now = now_ms();
        self.inner.fees.lock().unwrap().update(base_fee, now);
        tracing::debug!(base_fee, "base fee injected");
    }

    /// Return the current cached base fee, if any (ignores TTL).
    pub fn base_fee(&self) -> Option<u64> {
        self.inner.fees.lock().unwrap().base_fee()
    }

    /// Override the gas cache TTL (milliseconds).
    ///
    /// When gas is managed externally via [`set_base_fee`](Self::set_base_fee),
    /// the default 2s TTL may be too tight. Set this to match the poller's
    /// cadence with headroom (e.g. `tick_secs * 2 * 1000`).
    pub fn set_gas_ttl(&self, ttl_ms: u64) {
        self.inner.fees.lock().unwrap().set_ttl(ttl_ms);
        tracing::debug!(ttl_ms, "gas cache TTL updated");
    }

    // ── Cache control ────────────────────────────────────────────────

    /// Invalidate the fast cache layer (prices, funding, balances).
    ///
    /// Call on new-block events to ensure fresh data. Every client over
    /// this reader shares the cache, so this is cohort-wide.
    pub fn invalidate_fast_cache(&self) {
        self.inner.state.lock().unwrap().invalidate_fast_layer();
    }

    /// Invalidate all cached state.
    pub fn invalidate_all_cache(&self) {
        self.inner.state.lock().unwrap().invalidate_all();
    }

    // ── Reads ────────────────────────────────────────────────────────

    /// The USDC balance of `holder`, in human units, from the fast cache
    /// layer or one `balanceOf` call. Cached per token and holder, so a
    /// reader shared across signers keeps each balance apart.
    pub async fn balance_of(&self, holder: Address) -> Result<f64> {
        let now_ts = now_secs();
        let usdc = self.inner.deployments.usdc;
        let key = BalanceKey {
            token: usdc.into(),
            holder: holder.into(),
        };

        {
            let cache = self.inner.state.lock().unwrap();
            if let Some(balance) = cache.get_balance(&key, now_ts) {
                tracing::trace!(%holder, balance, "USDC balance cache hit");
                return Ok(balance);
            }
        }

        let raw: U256 = IERC20::new(usdc, &self.inner.provider)
            .balanceOf(holder)
            .call()
            .await?;
        let balance = usdc_from_atoms(raw, "USDC balance")?;
        tracing::debug!(%holder, balance, "USDC balance fetched");

        self.inner
            .state
            .lock()
            .unwrap()
            .put_balance(key, balance, now_ts);
        Ok(balance)
    }

    /// Get the USDC and ETH balances of an address in a single RPC call.
    ///
    /// Uses Multicall3 to bundle a `balanceOf` (USDC) and `getEthBalance`
    /// (native ETH) into one `eth_call`. The RPC provider charges 1 CU
    /// regardless of how many sub-calls the multicall executes.
    ///
    /// Returns `(usdc_balance, eth_balance)` where USDC is in human units
    /// (e.g. `100.0` = 100 USDC) and ETH is in wei.
    pub async fn get_balances(&self, address: Address) -> Result<(f64, U256)> {
        let results = self.get_balances_batch(&[address]).await?;
        Ok(results.into_iter().next().unwrap())
    }

    /// Get the USDC and ETH balances for multiple addresses in a single RPC call.
    ///
    /// Uses Multicall3 to bundle N × `balanceOf` + N × `getEthBalance` into
    /// one `eth_call`. For 10 addresses, this is 1 CU instead of 20.
    ///
    /// Returns a `Vec<(usdc_balance, eth_balance)>` in the same order as
    /// the input addresses.
    pub async fn get_balances_batch(&self, addresses: &[Address]) -> Result<Vec<(f64, U256)>> {
        if addresses.is_empty() {
            return Ok(Vec::new());
        }

        let usdc_addr = self.inner.deployments.usdc;
        let n = addresses.len();

        // Build sub-calls: N × USDC balanceOf + N × ETH getEthBalance
        let mut calls = Vec::with_capacity(2 * n);

        for &addr in addresses {
            // USDC balanceOf(addr)
            let calldata = IERC20::balanceOfCall { account: addr }.abi_encode();
            calls.push(IMulticall3::Call3 {
                target: usdc_addr,
                allowFailure: false,
                callData: calldata.into(),
            });
        }

        for &addr in addresses {
            // getEthBalance(addr) — Multicall3 built-in
            let calldata = IMulticall3::getEthBalanceCall { addr }.abi_encode();
            calls.push(IMulticall3::Call3 {
                target: MULTICALL3,
                allowFailure: false,
                callData: calldata.into(),
            });
        }

        let multicall = IMulticall3::new(MULTICALL3, &self.inner.provider);
        let results = multicall.aggregate3(calls).call().await?;

        if results.len() != 2 * n {
            return Err(ContractError::MulticallFailed {
                reason: format!(
                    "multicall returned {} results, expected {}",
                    results.len(),
                    2 * n
                ),
            }
            .into());
        }

        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            // Decode USDC balance (first N results)
            let usdc_result = &results[i];
            if !usdc_result.success {
                return Err(ContractError::MulticallFailed {
                    reason: format!("USDC balanceOf failed for address {}", addresses[i]),
                }
                .into());
            }
            let usdc_raw = U256::abi_decode(&usdc_result.returnData).map_err(|e| {
                ValidationError::DecodeFailed {
                    context: format!("failed to decode USDC balance: {e}"),
                }
            })?;
            let usdc = usdc_from_atoms(usdc_raw, "USDC balance")?;

            // Decode ETH balance (last N results)
            let eth_result = &results[n + i];
            if !eth_result.success {
                return Err(ContractError::MulticallFailed {
                    reason: format!("getEthBalance failed for address {}", addresses[i]),
                }
                .into());
            }
            let eth = U256::abi_decode(&eth_result.returnData).map_err(|e| {
                ValidationError::DecodeFailed {
                    context: format!("failed to decode ETH balance: {e}"),
                }
            })?;

            out.push((usdc, eth));
        }

        tracing::debug!(count = n, "batch balances fetched via multicall");
        Ok(out)
    }

    /// Get the oracle index price from a beacon contract.
    ///
    /// The beacon address is available from `MarketConfig.beacon` (returned
    /// by [`get_config`](crate::MarketReader::get_config)).
    ///
    /// Note: `index()` is a state-mutating function on-chain; this performs an
    /// `eth_call` (simulation) and does not send a transaction.
    pub async fn get_index_price(&self, beacon: Address) -> Result<f64> {
        self.index_price_at(beacon, BlockId::latest()).await
    }

    /// The beacon's `index()` at `block`, exact in X96; a zero index is a
    /// broken beacon, not a price.
    pub(super) async fn index_x96_at(&self, beacon: Address, block: BlockId) -> Result<U256> {
        let contract = IBeacon::new(beacon, &self.inner.provider);
        let index_x96: U256 = contract.index().block(block).call().await?;

        if index_x96.is_zero() {
            return Err(ValidationError::InvalidPrice {
                reason: "beacon returned zero index".into(),
            }
            .into());
        }
        Ok(index_x96)
    }

    /// [`Self::get_index_price`] at `block`.
    pub(super) async fn index_price_at(&self, beacon: Address, block: BlockId) -> Result<f64> {
        let index_x96 = self.index_x96_at(beacon, block).await?;
        let index = price_x96_to_f64(index_x96)?;
        Ok(index)
    }

    // ── Shared primitives ────────────────────────────────────────────

    /// A typed Multicall3 batch pinned to `block`. Add calls with `add`
    /// and read them with `aggregate`, which fails as a whole if any call
    /// reverts (map its error with
    /// [`multicall_error`](super::queries::multicall_error)).
    pub(super) fn multicall_at(
        &self,
        block: BlockId,
    ) -> MulticallBuilder<Empty, &RootProvider<Ethereum>, Ethereum> {
        self.inner
            .provider
            .multicall()
            .address(MULTICALL3)
            .block(block)
    }

    /// Resolve the lagged, reorg-safe block that snapshot reads pin to.
    ///
    /// Lagging [`SNAPSHOT_BLOCK_LAG`] blocks behind the head keeps every
    /// replica of a load-balanced endpoint able to serve the pinned state;
    /// a replica that still misses the header is a failed read
    /// ([`ContractError::BlockUnavailable`]), never a silent degrade.
    pub(super) async fn lagged_snapshot_block(&self) -> Result<(BlockContext, BlockId)> {
        let number = self
            .inner
            .provider
            .get_block_number()
            .await?
            .saturating_sub(SNAPSHOT_BLOCK_LAG);
        self.block_at(number).await
    }

    /// Resolve block `number` to the context and hash-pinned id reads use.
    /// A header the endpoint cannot serve is a failed read
    /// ([`ContractError::BlockUnavailable`]), never a fall-back to the head.
    pub(super) async fn block_at(&self, number: u64) -> Result<(BlockContext, BlockId)> {
        let block = self
            .inner
            .provider
            .get_block_by_number(number.into())
            .await?
            .ok_or(ContractError::BlockUnavailable { number })?;
        Ok((
            BlockContext {
                number: block.header.number,
                hash: block.header.hash,
                timestamp: block.header.timestamp,
            },
            BlockId::hash(block.header.hash),
        ))
    }

    /// Run an `eth_call` simulation from `from` to verify a transaction
    /// won't revert.
    ///
    /// `from` is a semantic input: the contract sees it as `msg.sender`, so
    /// a probe from an unfunded address can fail where the funded sender's
    /// transaction would succeed. When `gas_limit` is set, the simulation
    /// is capped at exactly the limit the transaction will broadcast with,
    /// so an execution that cannot finish inside a pinned limit fails
    /// preflight instead of burning the gas on-chain — the one failure a
    /// fixed limit like
    /// [`GasLimits::LIQUIDATE`](crate::hft::gas::GasLimits::LIQUIDATE)
    /// exists to prevent. Without a limit the node simulates with its
    /// default gas cap.
    pub(super) async fn preflight_call(
        &self,
        from: Address,
        to: Address,
        calldata: &Bytes,
        value: u128,
        gas_limit: Option<u64>,
    ) -> std::result::Result<(), TransactionError> {
        let tx = preflight_request(from, to, calldata, value, gas_limit);
        self.inner
            .provider
            .call(tx)
            .await
            .map_err(|e| classify_simulation_failure(&e, "eth_call"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::mock;

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

    /// Balances are cached per holder: another holder's read is another
    /// RPC, and the first holder's entry survives it.
    #[tokio::test]
    async fn balance_of_is_cached_per_holder() {
        let (a, b) = (Address::repeat_byte(0xa1), Address::repeat_byte(0xa2));
        let (chain, rpc) = mock::chain();
        rpc.call::<IERC20::balanceOfCall>(&U256::from(1_000_000u32));
        rpc.call::<IERC20::balanceOfCall>(&U256::from(2_500_000u32));

        assert_eq!(chain.balance_of(a).await.unwrap(), 1.0);
        assert_eq!(
            chain.balance_of(b).await.unwrap(),
            2.5,
            "another holder is another entry, not a hit"
        );
        assert_eq!(chain.balance_of(a).await.unwrap(), 1.0, "cache hit");
        assert_eq!(chain.balance_of(b).await.unwrap(), 2.5, "cache hit");
        assert!(rpc.is_drained());
    }
}
