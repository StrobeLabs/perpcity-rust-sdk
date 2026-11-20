//! The chain-scoped read handle, [`ChainReader`].

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::Ethereum;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{Empty, MulticallBuilder, Provider, RootProvider};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::BlockOverrides;
use alloy::sol_types::{SolCall, SolValue};
use alloy::transports::{BoxTransport, RpcError, TransportError};
use serde::{Deserialize, Serialize};

use crate::constants::{MULTICALL3, SNAPSHOT_BLOCK_LAG};
use crate::contracts::{IBeacon, IERC20, IMulticall3, v1};
use crate::convert::price_x96_to_f64;
use crate::errors::decode::decode_revert;
use crate::errors::{ContractError, PerpCityError, Result, TransactionError, ValidationError};
use crate::hft::gas::FeeCache;
use crate::hft::state_cache::{BalanceKey, StateCache, StateCacheConfig};
use crate::history::History;
use crate::math::BlockContext;
use crate::math::pricing::PricePair;
use crate::storage::{perp_emas_slot, stored_emas, v1_perp_emas_slot};
use crate::transport::fault::lacks_block;
use crate::transport::provider::HftTransport;
use crate::units::UsdcAtoms;

use super::queries::{Era, MarketImmutables};
use super::transactions::{classify_simulation_failure, preflight_request};
use super::{
    ARBITRUM_CHAIN_ID, ARBITRUM_POOL_MANAGER, ARBITRUM_SEPOLIA_CHAIN_ID,
    ARBITRUM_SEPOLIA_POOL_MANAGER, ARBITRUM_SEPOLIA_USDC, ARBITRUM_SEPOLIA_V1_PERP_FACTORY,
    ARBITRUM_SEPOLIA_V1_PERP_GUARD_HOOK, ARBITRUM_SEPOLIA_V1_WITHDRAWAL_CONTROLLER_IMPL,
    ARBITRUM_SEPOLIA_V022_PERP_GUARD_HOOK, ARBITRUM_USDC, ARBITRUM_V1_PERP_FACTORY,
    ARBITRUM_V1_PERP_GUARD_HOOK, ARBITRUM_V1_WITHDRAWAL_CONTROLLER_IMPL,
    ARBITRUM_V022_PERP_GUARD_HOOK, DEFAULT_GAS_TTL_MS, DEFAULT_PRIORITY_FEE, now_ms, now_secs,
};

/// The addresses every market on a chain shares, and the guard hook that
/// names each hooked build's pools, by which a market's [`Era`] is read
/// ([`Self::era_of`]).
///
/// A chain with no deployment of a build holds `Address::ZERO` for its
/// addresses, which no hooked pool names. An address is one chain's: the
/// same address is another contract on the other chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainDeployments {
    /// The collateral token.
    pub usdc: Address,
    /// The Uniswap V4 `PoolManager` every market's pool lives in (see
    /// `ARBITRUM_POOL_MANAGER` / `ARBITRUM_SEPOLIA_POOL_MANAGER`).
    pub pool_manager: Address,
    /// The `PerpGuardHook` in every `v0.2.2-upgradeable` pool.
    pub v022_perp_guard_hook: Address,
    /// The `PerpGuardHook` in every `v1.0.0` pool.
    pub v1_perp_guard_hook: Address,
    /// The `v1.0.0` `PerpFactory`.
    pub v1_perp_factory: Address,
    /// The `v1.0.0` `WithdrawalController` implementation, which each
    /// market's own controller clones.
    pub v1_withdrawal_controller_impl: Address,
}

impl ChainDeployments {
    /// Arbitrum One: Circle USDC, the mainnet `PoolManager`, and the
    /// `v0.2.2` and `v1.0.0` deployments.
    pub const ARBITRUM: Self = Self {
        usdc: ARBITRUM_USDC,
        pool_manager: ARBITRUM_POOL_MANAGER,
        v022_perp_guard_hook: ARBITRUM_V022_PERP_GUARD_HOOK,
        v1_perp_guard_hook: ARBITRUM_V1_PERP_GUARD_HOOK,
        v1_perp_factory: ARBITRUM_V1_PERP_FACTORY,
        v1_withdrawal_controller_impl: ARBITRUM_V1_WITHDRAWAL_CONTROLLER_IMPL,
    };

    /// Arbitrum Sepolia: the deployment's test USDC, the testnet
    /// `PoolManager`, the `v0.2.2` rehearsal factory's hook and the
    /// `v1.0.0` deployment.
    pub const ARBITRUM_SEPOLIA: Self = Self {
        usdc: ARBITRUM_SEPOLIA_USDC,
        pool_manager: ARBITRUM_SEPOLIA_POOL_MANAGER,
        v022_perp_guard_hook: ARBITRUM_SEPOLIA_V022_PERP_GUARD_HOOK,
        v1_perp_guard_hook: ARBITRUM_SEPOLIA_V1_PERP_GUARD_HOOK,
        v1_perp_factory: ARBITRUM_SEPOLIA_V1_PERP_FACTORY,
        v1_withdrawal_controller_impl: ARBITRUM_SEPOLIA_V1_WITHDRAWAL_CONTROLLER_IMPL,
    };

    /// The era of a market on this chain whose pool key names `hooks`: no
    /// hook is build `58b42b7`, this chain's `v1.0.0` or `v0.2.2` guard
    /// hook is that build, and any other hook is a build the SDK does not
    /// know (`None`), never a guess.
    pub fn era_of(&self, hooks: Address) -> Option<Era> {
        if hooks == Address::ZERO {
            Some(Era::Legacy)
        } else if hooks == self.v1_perp_guard_hook {
            Some(Era::V1)
        } else if hooks == self.v022_perp_guard_hook {
            Some(Era::Upgradeable)
        } else {
            None
        }
    }
}

/// One address's collateral and gas money, read in one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Balances {
    /// The chain's USDC.
    pub usdc: UsdcAtoms,
    /// Native ETH, in wei.
    pub eth_wei: U256,
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
    /// Makes no network calls, so `chain_id` is taken on trust;
    /// [`Self::checked`] asks the endpoint.
    pub fn new(transport: HftTransport, deployments: ChainDeployments, chain_id: u64) -> Self {
        let rpc_client = RpcClient::new(BoxTransport::new(transport.clone()), false);
        let provider = RootProvider::<Ethereum>::new(rpc_client);
        Self::from_parts(provider, transport, deployments, chain_id)
    }

    /// A reader for Arbitrum One, over [`ChainDeployments::ARBITRUM`].
    pub fn arbitrum(transport: HftTransport) -> Self {
        Self::new(transport, ChainDeployments::ARBITRUM, ARBITRUM_CHAIN_ID)
    }

    /// A reader for Arbitrum Sepolia, over
    /// [`ChainDeployments::ARBITRUM_SEPOLIA`].
    pub fn arbitrum_sepolia(transport: HftTransport) -> Self {
        Self::new(
            transport,
            ChainDeployments::ARBITRUM_SEPOLIA,
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

    /// This reader, once the endpoint's `eth_chainId` is the chain it was
    /// built for: `ChainReader::arbitrum(transport).checked().await?`.
    /// Refuses another chain with [`ContractError::WrongChain`], since its
    /// deployments' addresses would name another chain's contracts.
    pub async fn checked(self) -> Result<Self> {
        let served = self.inner.provider.get_chain_id().await?;
        if served != self.inner.chain_id {
            return Err(ContractError::WrongChain {
                expected: self.inner.chain_id,
                served,
            }
            .into());
        }
        Ok(self)
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

    /// The USDC balance of `holder`, from the fast cache layer or one
    /// `balanceOf` call. Cached per token and holder, so a reader shared
    /// across signers keeps each balance apart.
    pub async fn balance_of(&self, holder: Address) -> Result<UsdcAtoms> {
        let now_ts = now_secs();
        let usdc = self.inner.deployments.usdc;
        let key = BalanceKey {
            token: usdc.into(),
            holder: holder.into(),
        };

        {
            let cache = self.inner.state.lock().unwrap();
            if let Some(balance) = cache.get_balance(&key, now_ts) {
                tracing::trace!(%holder, %balance, "USDC balance cache hit");
                return Ok(balance);
            }
        }

        let raw: U256 = IERC20::new(usdc, &self.inner.provider)
            .balanceOf(holder)
            .call()
            .await?;
        let balance = UsdcAtoms::try_from(raw)?;
        tracing::debug!(%holder, %balance, "USDC balance fetched");

        self.inner
            .state
            .lock()
            .unwrap()
            .put_balance(key, balance, now_ts);
        Ok(balance)
    }

    /// An address's USDC and ETH in one `eth_call`: a Multicall3 bundle of
    /// `balanceOf` and `getEthBalance`.
    pub async fn get_balances(&self, address: Address) -> Result<Balances> {
        let [balances] = self.get_balances_batch(&[address]).await?[..] else {
            return Err(ContractError::MulticallFailed {
                reason: format!("no balances returned for {address}"),
            }
            .into());
        };
        Ok(balances)
    }

    /// The balances of many addresses in one `eth_call`, in their order:
    /// N `balanceOf` and N `getEthBalance` in one Multicall3 bundle.
    pub async fn get_balances_batch(&self, addresses: &[Address]) -> Result<Vec<Balances>> {
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
            let usdc = UsdcAtoms::try_from(usdc_raw)?;

            // Decode ETH balance (last N results)
            let eth_result = &results[n + i];
            if !eth_result.success {
                return Err(ContractError::MulticallFailed {
                    reason: format!("getEthBalance failed for address {}", addresses[i]),
                }
                .into());
            }
            let eth_wei = U256::abi_decode(&eth_result.returnData).map_err(|e| {
                ValidationError::DecodeFailed {
                    context: format!("failed to decode ETH balance: {e}"),
                }
            })?;

            out.push(Balances { usdc, eth_wei });
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

    /// The market's stored EMA pair at `block`, from its storage slot: the
    /// one read both earlier builds answer, since `v0.2.2` has no `emas()`.
    ///
    /// A transport failure is a typed, transient
    /// [`ContractError::StorageReadFailed`]; the node's own answer stays a
    /// response error so the caller's pinned-read mapping can name the
    /// block it is about.
    pub(super) async fn stored_emas_at(&self, perp: Address, block: BlockId) -> Result<PricePair> {
        self.emas_word_at(perp, perp_emas_slot(), block).await
    }

    /// [`Self::stored_emas_at`] on a `v1.0.0` market, whose pair is at
    /// slot 23 and which has no `emas()` view either.
    pub(super) async fn v1_stored_emas_at(
        &self,
        perp: Address,
        block: BlockId,
    ) -> Result<PricePair> {
        self.emas_word_at(perp, v1_perp_emas_slot(), block).await
    }

    async fn emas_word_at(&self, perp: Address, slot: U256, block: BlockId) -> Result<PricePair> {
        let word = self
            .storage_word_at(perp, slot, block, "stored EMAs at the market's slot")
            .await?;
        Ok(stored_emas(word))
    }

    /// One storage word of `account` at `block`. A transport failure is a
    /// typed, transient [`ContractError::StorageReadFailed`] naming
    /// `context`; the node's own answer stays a response error so the
    /// caller's pinned-read mapping can name the block it is about.
    pub(super) async fn storage_word_at(
        &self,
        account: Address,
        slot: U256,
        block: BlockId,
        context: &str,
    ) -> Result<U256> {
        self.inner
            .provider
            .get_storage_at(account, slot)
            .block_id(block)
            .await
            .map_err(|e| storage_read_error(e, context))
    }

    /// A `v1.0.0` beacon's `index()` at `block`: the value, exact in X96,
    /// and its full-width update timestamp.
    pub(super) async fn v1_index_at(
        &self,
        beacon: Address,
        block: BlockId,
    ) -> Result<(U256, U256)> {
        let observation = v1::IBeacon::new(beacon, &self.inner.provider)
            .index()
            .block(block)
            .call()
            .await?;
        Ok((observation.index, observation.updatedAt))
    }

    /// The index the market accepts at this block, with the market as the beacon's caller.
    pub(super) async fn v1_market_index_at(
        &self,
        perp: Address,
        beacon: Address,
        block: BlockId,
    ) -> Result<U256> {
        let call = IMulticall3::Call3 {
            target: perp,
            allowFailure: true,
            callData: v1::Perp::readBeaconIndexCall { beacon }.abi_encode().into(),
        };
        let rows = IMulticall3::new(MULTICALL3, &self.inner.provider)
            .aggregate3(vec![call])
            .block(block)
            .call()
            .await?;
        let [row] = rows.as_slice() else {
            return Err(ContractError::MulticallFailed {
                reason: format!("expected one beacon result, got {}", rows.len()),
            }
            .into());
        };
        if !row.success {
            return Err(row_revert(&row.returnData));
        }
        Ok(U256::from(
            v1::Perp::readBeaconIndexCall::abi_decode_returns_validate(&row.returnData)
                .map_err(alloy::contract::Error::from)?,
        ))
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
        self.preflight_call_in(from, to, calldata, value, gas_limit, None)
            .await
    }

    /// The block after the head, read now, for a `v1.0.0` trade's
    /// simulation.
    ///
    /// # Errors
    ///
    /// [`TransactionError::GasUnavailable`] (transient) when the endpoint
    /// returns no head block, or the read's failure.
    pub(super) async fn next_block(&self) -> Result<NextBlock> {
        let head = self
            .inner
            .provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await?
            .ok_or_else(|| TransactionError::GasUnavailable {
                reason: "the endpoint returned no head block".into(),
            })?;
        Ok(NextBlock::after(head.header.number, head.header.timestamp))
    }

    /// [`Self::preflight_call`] in `next` when it is given (see
    /// [`Self::in_next_block`]).
    pub(super) async fn preflight_call_in(
        &self,
        from: Address,
        to: Address,
        calldata: &Bytes,
        value: u128,
        gas_limit: Option<u64>,
        next: Option<&NextBlock>,
    ) -> std::result::Result<(), TransactionError> {
        let tx = preflight_request(from, to, calldata, value, gas_limit);
        let provider = &self.inner.provider;
        match next {
            None => provider
                .call(tx)
                .await
                .map(drop)
                .map_err(|e| classify_simulation_failure(&e, "eth_call")),
            Some(next) => {
                self.in_next_block("eth_call", next, |next| {
                    let tx = tx.clone();
                    async move {
                        provider
                            .call(tx)
                            .block(next.state())
                            .with_block_overrides(next.overrides())
                            .await
                            .map(drop)
                    }
                })
                .await
            }
        }
    }

    /// `eth_estimateGas` for the call from `from`, in `next` when it is
    /// given (see [`Self::in_next_block`]), with a contract revert decoded.
    pub(super) async fn estimate_gas_in(
        &self,
        from: Address,
        to: Address,
        calldata: &Bytes,
        value: u128,
        next: Option<&NextBlock>,
    ) -> std::result::Result<u64, TransactionError> {
        let tx = preflight_request(from, to, calldata, value, None);
        let provider = &self.inner.provider;
        match next {
            None => provider
                .estimate_gas(tx)
                .await
                .map_err(|e| classify_simulation_failure(&e, "eth_estimateGas")),
            Some(next) => {
                self.in_next_block("eth_estimateGas", next, |next| {
                    let tx = tx.clone();
                    async move {
                        provider
                            .estimate_gas(tx)
                            .block(next.state())
                            .with_block_overrides(next.overrides())
                            .await
                    }
                })
                .await
            }
        }
    }

    /// The preflight of a call whose gas limit is fixed, by the caller or
    /// by the stuck transaction a replacement signs again: capped at
    /// `gas_limit`, in `next` when it is given. With no endpoint that takes
    /// block overrides for `eth_call`, the preflight runs at the endpoint's
    /// own block, since the limit sent does not depend on it. A capped call
    /// that reverts may only have run short of gas inside a token transfer,
    /// so it is run once more without the cap.
    ///
    /// # Errors
    ///
    /// [`TransactionError::GasLimitTooLow`] when the call succeeds without
    /// the cap; else the capped call's failure, as
    /// [`classify_simulation_failure`] sorts it.
    pub(super) async fn preflight_at_limit(
        &self,
        from: Address,
        to: Address,
        calldata: &Bytes,
        value: u128,
        gas_limit: u64,
        next: Option<&NextBlock>,
    ) -> std::result::Result<(), TransactionError> {
        let mut next = next;
        let mut capped = self
            .preflight_call_in(from, to, calldata, value, Some(gas_limit), next)
            .await;
        if next.is_some()
            && matches!(capped, Err(TransactionError::GasUnavailable { .. }))
            && !self.inner.transport.simulates_with_overrides("eth_call")
        {
            tracing::warn!(
                gas_limit,
                "fixed-limit preflight runs at the endpoint's own block: no endpoint takes block overrides"
            );
            next = None;
            capped = self
                .preflight_call_in(from, to, calldata, value, Some(gas_limit), None)
                .await;
        }
        let Err(TransactionError::SimulationReverted { error_name, .. }) = &capped else {
            return capped;
        };
        match self
            .preflight_call_in(from, to, calldata, value, None, next)
            .await
        {
            Ok(()) => Err(TransactionError::GasLimitTooLow {
                gas_limit,
                error_name: error_name.clone(),
            }),
            Err(_) => capped,
        }
    }

    /// `simulate`, a simulation with block overrides, in `next`. Refused as
    /// [`TransactionError::GasUnavailable`] (transient) when no endpoint
    /// takes block overrides for `method`, since a simulation at the head's
    /// time can skip a `v1.0.0` trade's accrual. A node without the head
    /// `next` pins, which the transport passes to another endpoint first,
    /// is answered by reading the head again and simulating once more;
    /// a second miss is `GasUnavailable` too.
    async fn in_next_block<T, F, Fut>(
        &self,
        method: &'static str,
        next: &NextBlock,
        simulate: F,
    ) -> std::result::Result<T, TransactionError>
    where
        F: Fn(NextBlock) -> Fut,
        Fut: Future<Output = std::result::Result<T, TransportError>>,
    {
        if !self.inner.transport.simulates_with_overrides(method) {
            tracing::error!(
                method,
                "no endpoint takes block overrides; a v1 trade's gas cannot be simulated in the next block"
            );
            return Err(TransactionError::GasUnavailable {
                reason: format!(
                    "no endpoint takes block overrides for {method}, which a v1 trade's gas needs"
                ),
            });
        }
        let mut next = *next;
        let mut answer = simulate(next).await;
        if answer.as_ref().is_err_and(lacks_pinned_block) {
            match self.next_block().await {
                Ok(head) => {
                    tracing::debug!(
                        missing = next.head,
                        head = head.head,
                        method,
                        "simulation's pinned head missing; read the head again"
                    );
                    next = head;
                    answer = simulate(next).await;
                }
                Err(error) => {
                    tracing::debug!(%error, method, "head read after a missing pinned head failed");
                }
            }
        }
        answer.map_err(|error| {
            if lacks_pinned_block(&error) {
                TransactionError::GasUnavailable {
                    reason: format!("{method} at block {} failed: {error}", next.head),
                }
            } else {
                classify_simulation_failure(&error, method)
            }
        })
    }
}

/// The block a `v1.0.0` trade is simulated in before its send: the head's
/// state, with the next block's number and a timestamp one second after
/// the head's. A trade accrues its market's rates, walking the initialized
/// ticks of the utilization band, only in a second other than the market's
/// last touch, and an endpoint runs a simulation at the head's own time,
/// so without the override a simulation in the second of the last touch
/// skips work the mined trade does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct NextBlock {
    head: u64,
    head_time: u64,
}

impl NextBlock {
    /// The block after the head numbered `head`, stamped `head_time`.
    pub(super) fn after(head: u64, head_time: u64) -> Self {
        Self { head, head_time }
    }

    /// The head, whose state the simulation reads.
    fn state(&self) -> BlockId {
        BlockId::number(self.head)
    }

    /// The next block's number and time.
    fn overrides(&self) -> BlockOverrides {
        BlockOverrides {
            number: Some(U256::from(self.head + 1)),
            time: Some(self.head_time + 1),
            ..BlockOverrides::default()
        }
    }
}

/// Whether a simulation's `error` is the node's answer that it lacks the
/// block the simulation pinned ([`lacks_block`]).
fn lacks_pinned_block(error: &TransportError) -> bool {
    error
        .as_error_resp()
        .is_some_and(|resp| lacks_block(&resp.message))
}

/// A raw storage read's failure, classified: the node answering with an
/// error is kept as the response it was, so the caller can read a pruned
/// or missing block out of its message; anything else is the transport
/// failing, retryable, and typed as the storage read it interrupted.
fn storage_read_error(error: TransportError, context: &str) -> PerpCityError {
    match error {
        RpcError::ErrorResp(_) => alloy::contract::Error::TransportError(error).into(),
        other => ContractError::StorageReadFailed {
            context: context.into(),
            source: Some(Arc::new(other)),
        }
        .into(),
    }
}

pub(super) fn row_revert(bytes: &Bytes) -> PerpCityError {
    let data = bytes.to_string();
    match decode_revert(&data) {
        Some((error_name, selector)) => TransactionError::SimulationReverted {
            selector: selector.into(),
            error_name,
            revert_data: Some(data),
        }
        .into(),
        None => ContractError::RevertedWithoutSelector {
            data: bytes.clone(),
        }
        .into(),
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;
    use alloy::rpc::json_rpc::ErrorPayload;
    use alloy::transports::TransportErrorKind;

    use super::*;
    use crate::client::mock;

    /// The next block, as a public Arbitrum Sepolia node takes it in the
    /// param shape the simulations send: code that returns `TIMESTAMP`, run
    /// with the overrides, gives the head's time plus one, and
    /// `eth_estimateGas` takes the same overrides. (`NUMBER` is the L1
    /// block number on Arbitrum, which the number override does not set.)
    #[tokio::test]
    #[ignore = "network: simulates on a public Arbitrum Sepolia node"]
    async fn a_public_node_runs_the_next_block() {
        use alloy::rpc::types::state::{AccountOverride, StateOverride};

        let url = std::env::var("SEPOLIA_FORK_URL")
            .unwrap_or_else(|_| "https://sepolia-rollup.arbitrum.io/rpc".to_string());
        let transport = HftTransport::new(
            crate::TransportConfig::builder()
                .shared_endpoint(&url)
                .build()
                .unwrap(),
        )
        .unwrap();
        let chain = ChainReader::arbitrum_sepolia(transport);
        let next = chain.next_block().await.unwrap();
        let probe = address!("00000000000000000000000000000000000042ab");
        // TIMESTAMP, PUSH0, MSTORE, PUSH1 32, PUSH0, RETURN.
        let code = Bytes::from_static(&[0x42, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3]);
        let state: StateOverride = [(
            probe,
            AccountOverride {
                code: Some(code),
                ..AccountOverride::default()
            },
        )]
        .into_iter()
        .collect();
        let tx = preflight_request(Address::ZERO, probe, &Bytes::new(), 0, None);
        let word = chain
            .provider()
            .call(tx)
            .block(next.state())
            .overrides(state)
            .with_block_overrides(next.overrides())
            .await
            .unwrap();
        assert_eq!(U256::from_be_slice(&word), U256::from(next.head_time + 1));
        let gas = chain
            .estimate_gas_in(Address::ZERO, probe, &Bytes::new(), 0, Some(&next))
            .await
            .unwrap();
        assert!(gas >= 21_000, "{gas}");
    }

    #[tokio::test]
    async fn beacon_read_preserves_node_errors() {
        let (chain, rpc) = mock::chain();
        let data = Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef, 0x42]);
        rpc.aggregate3(vec![IMulticall3::Result {
            success: false,
            returnData: data.clone(),
        }]);
        let err = chain
            .v1_market_index_at(Address::ZERO, Address::ZERO, BlockId::latest())
            .await
            .unwrap_err();
        assert!(
            matches!(err, PerpCityError::Transaction(TransactionError::SimulationReverted {
            selector, revert_data: Some(payload), ..
        }) if selector.0 == [0xde, 0xad, 0xbe, 0xef] && payload == data.to_string())
        );

        for length in 0..4 {
            let payload = data.slice(..length);
            rpc.aggregate3(vec![IMulticall3::Result {
                success: false,
                returnData: payload.clone(),
            }]);
            let err = chain
                .v1_market_index_at(Address::ZERO, Address::ZERO, BlockId::latest())
                .await
                .unwrap_err();
            assert!(!err.is_transient());
            assert!(err.revert_class().is_none());
            assert!(matches!(err, PerpCityError::Contract(
                ContractError::RevertedWithoutSelector { data }
            ) if data == payload));
        }

        rpc.fails("execution reverted");
        let err = chain
            .v1_market_index_at(Address::ZERO, Address::ZERO, BlockId::latest())
            .await
            .unwrap_err();
        assert!(
            matches!(err, PerpCityError::Abi(alloy::contract::Error::TransportError(RpcError::ErrorResp(resp))) if resp.message == "execution reverted")
        );
        // An exhausted mock transport cannot deliver an RPC response.
        let err = chain
            .v1_market_index_at(Address::ZERO, Address::ZERO, BlockId::latest())
            .await
            .unwrap_err();
        assert!(
            matches!(err, PerpCityError::Abi(alloy::contract::Error::TransportError(err)) if err.as_error_resp().is_none())
        );
    }

    /// A dropped connection on the slot read retries; the node's own
    /// answer is left for the pinned-read mapping to classify.
    #[test]
    fn a_storage_read_cut_off_by_the_transport_is_transient() {
        let cut = storage_read_error(TransportErrorKind::custom_str("connection reset"), "slot");
        assert!(
            matches!(
                cut,
                PerpCityError::Contract(ContractError::StorageReadFailed {
                    source: Some(_),
                    ..
                })
            ),
            "{cut}"
        );
        assert!(cut.is_transient());

        let answered = storage_read_error(
            RpcError::ErrorResp(ErrorPayload {
                code: -32000,
                message: "header not found".into(),
                data: None,
            }),
            "slot",
        );
        assert!(
            matches!(
                answered,
                PerpCityError::Abi(alloy::contract::Error::TransportError(RpcError::ErrorResp(
                    _
                )))
            ),
            "{answered}"
        );
    }

    /// The endpoint's chain is asked once; the chain the reader was built
    /// for passes, and another is refused naming both.
    #[tokio::test]
    async fn checked_refuses_an_endpoint_on_another_chain() {
        let (chain, rpc) = mock::chain();
        rpc.quantity(mock::CHAIN_ID);
        let chain = chain.checked().await.unwrap();
        assert!(rpc.is_drained());

        rpc.quantity(421_614);
        let refused = chain.checked().await.unwrap_err();
        assert!(
            matches!(
                refused,
                PerpCityError::Contract(ContractError::WrongChain {
                    expected: mock::CHAIN_ID,
                    served: 421_614,
                })
            ),
            "{refused}"
        );
        assert!(!refused.is_transient());
        assert!(rpc.is_drained());
    }

    #[test]
    fn chain_deployments_serde_roundtrip() {
        let deployments = ChainDeployments::ARBITRUM;
        let json = serde_json::to_string(&deployments).unwrap();
        let recovered: ChainDeployments = serde_json::from_str(&json).unwrap();
        assert_eq!(deployments, recovered);
    }

    /// The one rule, on each known chain: no hook is `Legacy`, the chain's
    /// `v1.0.0` hook is `V1`, its `v0.2.2` hook is `Upgradeable`, and any
    /// other hook is unknown. A hook is one chain's: the other chain's
    /// hooks are unknown here.
    #[test]
    fn the_era_is_the_chain_s_hook() {
        let one = ChainDeployments::ARBITRUM;
        let sepolia = ChainDeployments::ARBITRUM_SEPOLIA;
        for chain in [one, sepolia] {
            assert_eq!(chain.era_of(Address::ZERO), Some(Era::Legacy));
            assert_eq!(chain.era_of(chain.v1_perp_guard_hook), Some(Era::V1));
            assert_eq!(
                chain.era_of(chain.v022_perp_guard_hook),
                Some(Era::Upgradeable)
            );
            assert_eq!(chain.era_of(Address::repeat_byte(0x0a)), None);
        }
        assert_eq!(
            one.v1_perp_guard_hook,
            address!("63adaa672d90B91e361C3f52552ee885A6D6AaA0")
        );
        assert_eq!(
            one.v022_perp_guard_hook,
            address!("561Ae0C8d4f3AeeaeB92Fa892281A4dA97De0AA0")
        );
        assert_eq!(
            sepolia.v1_perp_guard_hook,
            address!("9FCBEc9019d37780A0c21573cC4F556691252aA0")
        );
        assert_eq!(
            sepolia.v022_perp_guard_hook,
            address!("f03fF5e350EA455B452a83ef89a3bcbcD6F98aa0")
        );
        let absent = ChainDeployments {
            v022_perp_guard_hook: Address::ZERO,
            v1_perp_guard_hook: Address::ZERO,
            ..one
        };
        assert_eq!(absent.era_of(Address::ZERO), Some(Era::Legacy));
        assert_eq!(one.era_of(sepolia.v1_perp_guard_hook), None);
        assert_eq!(one.era_of(sepolia.v022_perp_guard_hook), None);
        assert_eq!(sepolia.era_of(one.v1_perp_guard_hook), None);
        assert_eq!(sepolia.era_of(one.v022_perp_guard_hook), None);
    }

    /// The known chains' readers carry their chain's deployments.
    #[test]
    fn the_known_chains_carry_their_deployments() {
        let one = ChainReader::arbitrum(mock::transport());
        assert_eq!(one.deployments(), &ChainDeployments::ARBITRUM);
        assert_eq!(one.chain_id(), ARBITRUM_CHAIN_ID);
        let sepolia = ChainReader::arbitrum_sepolia(mock::transport());
        assert_eq!(sepolia.deployments(), &ChainDeployments::ARBITRUM_SEPOLIA);
        assert_eq!(sepolia.chain_id(), ARBITRUM_SEPOLIA_CHAIN_ID);
    }

    /// Balances are cached per holder: another holder's read is another
    /// RPC, and the first holder's entry survives it.
    #[tokio::test]
    async fn balance_of_is_cached_per_holder() {
        let (a, b) = (Address::repeat_byte(0xa1), Address::repeat_byte(0xa2));
        let (chain, rpc) = mock::chain();
        rpc.call::<IERC20::balanceOfCall>(&U256::from(1_000_000u32));
        rpc.call::<IERC20::balanceOfCall>(&U256::from(2_500_000u32));

        let (one, two_and_a_half) = (UsdcAtoms::new(1_000_000), UsdcAtoms::new(2_500_000));
        assert_eq!(chain.balance_of(a).await.unwrap(), one);
        assert_eq!(
            chain.balance_of(b).await.unwrap(),
            two_and_a_half,
            "another holder is another entry, not a hit"
        );
        assert_eq!(chain.balance_of(a).await.unwrap(), one, "cache hit");
        assert_eq!(
            chain.balance_of(b).await.unwrap(),
            two_and_a_half,
            "cache hit"
        );
        assert!(rpc.is_drained());
    }
}
