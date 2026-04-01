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

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::network::{Ethereum, EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, B256, Bytes, I256, U256};
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::transports::BoxTransport;

use crate::constants::SCALE_1E6;
use crate::contracts::{IBeacon, IERC20, IFees, IMarginRatios, PerpManager};
use crate::convert::{
    leverage_to_margin_ratio, margin_ratio_to_leverage, scale_from_6dec, scale_to_6dec,
};
use crate::errors::{PerpCityError, Result};
use crate::hft::gas::{GasCache, GasLimits, Urgency};
use crate::hft::pipeline::{PipelineConfig, TxPipeline, TxRequest};
use crate::hft::state_cache::{CachedBounds, CachedFees, StateCache, StateCacheConfig};
use crate::math::tick::{align_tick_down, align_tick_up, price_to_tick};
use crate::transport::provider::HftTransport;
use crate::types::{
    AdjustMarginParams, AdjustMarginResult, AdjustNotionalParams, AdjustNotionalResult, Bounds,
    CloseParams, CloseResult, Deployments, Fees, LiveDetails, OpenInterest, OpenMakerParams,
    OpenMakerQuote, OpenResult, OpenTakerParams, OpenTakerQuote, PerpData, SwapQuote,
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

/// Default receipt polling timeout.
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum USDC approval amount (2^256 - 1).
const MAX_APPROVAL: U256 = U256::MAX;

/// SCALE_1E6 as f64, used for converting on-chain fixed-point values.
const SCALE_F64: f64 = SCALE_1E6 as f64;

// ── From impls for cache↔client type bridging ────────────────────────

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
    gas_cache: Mutex<GasCache>,
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
        let rpc_client = RpcClient::new(boxed, true);
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
            gas_cache: Mutex::new(GasCache::new(DEFAULT_GAS_TTL_MS, DEFAULT_PRIORITY_FEE)),
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
            .ok_or_else(|| PerpCityError::GasPriceUnavailable {
                reason: "latest block not found".into(),
            })?;

        let base_fee =
            header
                .header
                .base_fee_per_gas
                .ok_or_else(|| PerpCityError::GasPriceUnavailable {
                    reason: "block has no base fee (pre-EIP-1559?)".into(),
                })?;

        let now = now_ms();
        self.gas_cache.lock().unwrap().update(base_fee, now);
        tracing::debug!(base_fee, "gas cache refreshed");
        Ok(())
    }

    /// Inject a base fee from an external source (e.g. a shared poller).
    ///
    /// Updates the gas cache as if `refresh_gas` had been called, but without
    /// any RPC calls. The cache TTL is reset to now.
    pub fn set_base_fee(&self, base_fee: u64) {
        let now = now_ms();
        self.gas_cache.lock().unwrap().update(base_fee, now);
        tracing::debug!(base_fee, "base fee injected");
    }

    /// Return the current cached base fee, if any (ignores TTL).
    ///
    /// Intended for reading the base fee after `refresh_gas` in order to
    /// distribute it to other clients via [`set_base_fee`](Self::set_base_fee).
    pub fn base_fee(&self) -> Option<u64> {
        self.gas_cache.lock().unwrap().base_fee()
    }

    /// Override the gas cache TTL (milliseconds).
    ///
    /// When gas is managed externally via [`set_base_fee`](Self::set_base_fee),
    /// the default 2s TTL may be too tight. Set this to match the poller's
    /// cadence with headroom (e.g. `tick_secs * 2 * 1000`).
    pub fn set_gas_ttl(&self, ttl_ms: u64) {
        self.gas_cache.lock().unwrap().set_ttl(ttl_ms);
        tracing::debug!(ttl_ms, "gas cache TTL updated");
    }

    // ── Write operations ─────────────────────────────────────────────

    /// Open a taker (long/short) position.
    ///
    /// Returns an [`OpenResult`] with the position ID and entry deltas
    /// parsed from the `PositionOpened` event, so callers can construct
    /// position tracking data without a follow-up RPC read.
    ///
    /// # Errors
    ///
    /// Returns [`PerpCityError::TxReverted`] if the transaction reverts,
    /// or [`PerpCityError::EventNotFound`] if the `PositionOpened` event
    /// is missing from the receipt.
    pub async fn open_taker(
        &self,
        perp_id: B256,
        params: &OpenTakerParams,
        urgency: Urgency,
    ) -> Result<OpenResult> {
        let margin_scaled = scale_to_6dec(params.margin)?;
        if margin_scaled <= 0 {
            return Err(PerpCityError::InvalidMargin {
                reason: format!("margin must be positive, got {}", params.margin),
            });
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

        tracing::info!(%perp_id, margin = params.margin, leverage = params.leverage, is_long = params.is_long, ?urgency, "opening taker position");

        let receipt = self
            .send_tx(
                self.deployments.perp_manager,
                calldata,
                GasLimits::OPEN_TAKER,
                urgency,
            )
            .await?;

        let result = parse_open_result(&receipt)?;
        tracing::info!(%perp_id, pos_id = %result.pos_id, perp_delta = result.perp_delta, usd_delta = result.usd_delta, "taker position opened");
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
            return Err(PerpCityError::InvalidMargin {
                reason: format!("margin must be positive, got {}", params.margin),
            });
        }

        let tick_lower = align_tick_down(
            price_to_tick(params.price_lower)?,
            crate::constants::TICK_SPACING,
        );
        let tick_upper = align_tick_up(
            price_to_tick(params.price_upper)?,
            crate::constants::TICK_SPACING,
        );

        if tick_lower >= tick_upper {
            return Err(PerpCityError::InvalidTickRange {
                lower: tick_lower,
                upper: tick_upper,
            });
        }

        // Liquidity must fit in u120 on-chain
        let liquidity: u128 = params.liquidity;
        let max_u120: u128 = (1u128 << 120) - 1;
        if liquidity > max_u120 {
            return Err(PerpCityError::Overflow {
                context: format!("liquidity {} exceeds uint120 max", liquidity),
            });
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

        tracing::info!(%perp_id, margin = params.margin, tick_lower, tick_upper, ?urgency, "opening maker position");

        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let calldata = contract
            .openMakerPos(perp_id, wire_params)
            .calldata()
            .clone();

        let receipt = self
            .send_tx(
                self.deployments.perp_manager,
                calldata,
                GasLimits::OPEN_MAKER,
                urgency,
            )
            .await?;

        let result = parse_open_result(&receipt)?;
        tracing::info!(%perp_id, pos_id = %result.pos_id, perp_delta = result.perp_delta, usd_delta = result.usd_delta, "maker position opened");
        Ok(result)
    }

    /// Close a position (taker or maker).
    ///
    /// Returns a [`CloseResult`] with the transaction hash and optional
    /// remaining position ID (for partial closes).
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

        tracing::info!(pos_id = %pos_id, ?urgency, "closing position");

        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let calldata = contract.closePosition(wire_params).calldata().clone();

        let receipt = self
            .send_tx(
                self.deployments.perp_manager,
                calldata,
                GasLimits::CLOSE_POSITION,
                urgency,
            )
            .await?;

        let result = parse_close_result(&receipt, pos_id)?;
        tracing::info!(pos_id = %pos_id, was_liquidated = result.was_liquidated, net_margin = result.net_margin, "position closed");
        Ok(result)
    }

    /// Adjust the notional exposure of a taker position.
    ///
    /// - `usd_delta > 0`: receive USD by selling perp tokens (reduce exposure)
    /// - `usd_delta < 0`: spend USD to buy perp tokens (increase exposure)
    pub async fn adjust_notional(
        &self,
        pos_id: U256,
        params: &AdjustNotionalParams,
        urgency: Urgency,
    ) -> Result<AdjustNotionalResult> {
        let usd_delta_scaled = scale_to_6dec(params.usd_delta)?;

        let wire_params = PerpManager::AdjustNotionalParams {
            posId: pos_id,
            usdDelta: I256::try_from(usd_delta_scaled).map_err(|_| PerpCityError::Overflow {
                context: format!("usd_delta {} overflows I256", usd_delta_scaled),
            })?,
            perpLimit: params.perp_limit,
        };

        tracing::info!(pos_id = %pos_id, usd_delta = params.usd_delta, ?urgency, "adjusting notional");

        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let calldata = contract.adjustNotional(wire_params).calldata().clone();

        let receipt = self
            .send_tx(
                self.deployments.perp_manager,
                calldata,
                GasLimits::ADJUST_NOTIONAL,
                urgency,
            )
            .await?;

        let result = parse_adjust_result(&receipt)?;
        tracing::info!(pos_id = %pos_id, new_perp_delta = result.new_perp_delta, "notional adjusted");
        Ok(result)
    }

    /// Add or remove margin from a position.
    ///
    /// - `margin_delta > 0`: deposit more margin
    /// - `margin_delta < 0`: withdraw margin
    pub async fn adjust_margin(
        &self,
        pos_id: U256,
        params: &AdjustMarginParams,
        urgency: Urgency,
    ) -> Result<AdjustMarginResult> {
        let delta_scaled = scale_to_6dec(params.margin_delta)?;

        let wire_params = PerpManager::AdjustMarginParams {
            posId: pos_id,
            marginDelta: I256::try_from(delta_scaled).map_err(|_| PerpCityError::Overflow {
                context: format!("margin_delta {} overflows I256", delta_scaled),
            })?,
        };

        tracing::info!(pos_id = %pos_id, margin_delta = params.margin_delta, ?urgency, "adjusting margin");

        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let calldata = contract.adjustMargin(wire_params).calldata().clone();

        let receipt = self
            .send_tx(
                self.deployments.perp_manager,
                calldata,
                GasLimits::ADJUST_MARGIN,
                urgency,
            )
            .await?;

        let result = parse_margin_result(&receipt)?;
        tracing::info!(pos_id = %pos_id, new_margin = result.new_margin, "margin adjusted");
        Ok(result)
    }

    /// Ensure USDC is approved for the PerpManager to spend.
    ///
    /// Checks current allowance and only sends an `approve` transaction
    /// if the allowance is below `min_amount`. Approves for `U256::MAX`
    /// (infinite approval) to avoid repeated approve calls.
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

        tracing::info!(allowance = %allowance, min_amount = %min_amount, "approving USDC");

        let calldata = usdc
            .approve(self.deployments.perp_manager, MAX_APPROVAL)
            .calldata()
            .clone();

        let receipt = self
            .send_tx(
                self.deployments.usdc,
                calldata,
                GasLimits::APPROVE,
                Urgency::Normal,
            )
            .await?;

        tracing::info!(tx_hash = %receipt.transaction_hash, "USDC approved");
        Ok(Some(receipt.transaction_hash))
    }

    // ── Read operations ──────────────────────────────────────────────

    /// Get the full perp configuration, fees, and bounds for a market.
    ///
    /// Uses the [`StateCache`] for fees and bounds (60s TTL). The perp
    /// config itself is always fetched fresh (it's cheap and rarely changes).
    pub async fn get_perp_config(&self, perp_id: B256) -> Result<PerpData> {
        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);

        // Fetch perp config — sol!(rpc) returns the struct directly
        let config: PerpManager::PerpConfig = contract.cfgs(perp_id).call().await?;

        // Zero beacon means the perp was never created
        if config.beacon == Address::ZERO {
            return Err(PerpCityError::PerpNotFound { perp_id });
        }

        let beacon = config.beacon;

        // Fetch mark price via TWAP (short window = ~current price)
        let sqrt_price_x96: U256 = contract
            .timeWeightedAvgSqrtPriceX96(perp_id, 1)
            .call()
            .await?;
        let mark = crate::convert::sqrt_price_x96_to_price(sqrt_price_x96)?;

        let now_ts = now_secs();
        let fees_addr: [u8; 20] = config.fees.into();

        // Try cache for fees
        let fees = {
            let cache = self.state_cache.lock().unwrap();
            cache.get_fees(&fees_addr, now_ts).cloned()
        };

        let fees = match fees {
            Some(cached) => Fees::from(cached),
            None => {
                let fees = self.fetch_fees(&config).await?;
                let mut cache = self.state_cache.lock().unwrap();
                cache.put_fees(fees_addr, CachedFees::from(fees), now_ts);
                fees
            }
        };

        // Try cache for bounds
        let ratios_addr: [u8; 20] = config.marginRatios.into();
        let bounds = {
            let cache = self.state_cache.lock().unwrap();
            cache.get_bounds(&ratios_addr, now_ts).cloned()
        };

        let bounds = match bounds {
            Some(cached) => Bounds::from(cached),
            None => {
                let bounds = self.fetch_bounds(&config).await?;
                let mut cache = self.state_cache.lock().unwrap();
                cache.put_bounds(ratios_addr, CachedBounds::from(bounds), now_ts);
                bounds
            }
        };

        Ok(PerpData {
            id: perp_id,
            tick_spacing: i24_to_i32(config.key.tickSpacing),
            mark,
            beacon,
            bounds,
            fees,
        })
    }

    /// Get perp data: beacon, tick spacing, and current mark price.
    ///
    /// Lighter-weight than [`Self::get_perp_config`] — skips fees/bounds lookups.
    pub async fn get_perp_data(&self, perp_id: B256) -> Result<(Address, i32, f64)> {
        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let config: PerpManager::PerpConfig = contract.cfgs(perp_id).call().await?;

        let sqrt_price_x96: U256 = contract
            .timeWeightedAvgSqrtPriceX96(perp_id, 1)
            .call()
            .await?;
        let mark = crate::convert::sqrt_price_x96_to_price(sqrt_price_x96)?;

        Ok((config.beacon, i24_to_i32(config.key.tickSpacing), mark))
    }

    /// Get an on-chain position by its NFT token ID.
    ///
    /// Returns the raw contract position struct. Use [`crate::math::position`]
    /// functions to compute derived values (entry price, PnL, etc.).
    pub async fn get_position(&self, pos_id: U256) -> Result<PerpManager::Position> {
        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let pos: PerpManager::Position = contract.positions(pos_id).call().await?;

        // Check if position exists (empty perpId = uninitialized)
        if pos.perpId == B256::ZERO {
            return Err(PerpCityError::PositionNotFound { pos_id });
        }

        Ok(pos)
    }

    /// Get all position IDs owned by an address.
    ///
    /// Iterates through all minted position NFTs (1..nextPosId) and returns
    /// those owned by `owner`. Burned or non-existent tokens are skipped.
    ///
    /// **Note:** This is O(n) in total positions ever minted. For high-throughput
    /// use cases, prefer the bot API's position endpoints instead.
    pub async fn get_positions_by_owner(&self, owner: Address) -> Result<Vec<U256>> {
        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let next_pos_id: U256 = contract.nextPosId().call().await?;

        let total: u64 = next_pos_id
            .try_into()
            .map_err(|_| PerpCityError::Overflow {
                context: "nextPosId exceeds u64".into(),
            })?;
        if total <= 1 {
            return Ok(vec![]);
        }

        let mut owned = Vec::new();
        for id in 1..total {
            let pos_id = U256::from(id);
            // ownerOf reverts for burned/non-existent tokens — those
            // surface as contract errors, which we skip. Other transport
            // errors propagate so network failures aren't silently ignored.
            match contract.ownerOf(pos_id).call().await {
                Ok(addr) if addr == owner => owned.push(pos_id),
                Ok(_) => {}
                Err(e @ alloy::contract::Error::TransportError(_)) => return Err(e.into()),
                Err(_) => {} // burned or non-existent token
            }
        }

        Ok(owned)
    }

    /// Get the current mark price for a perp (TWAP with 1-second lookback).
    ///
    /// Uses the fast cache layer (2s TTL).
    pub async fn get_mark_price(&self, perp_id: B256) -> Result<f64> {
        let now_ts = now_secs();
        let perp_bytes: [u8; 32] = perp_id.into();

        // Check cache
        {
            let cache = self.state_cache.lock().unwrap();
            if let Some(price) = cache.get_mark_price(&perp_bytes, now_ts) {
                tracing::trace!(%perp_id, price, "mark price cache hit");
                return Ok(price);
            }
        }

        // Fetch from chain
        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let sqrt_price_x96: U256 = contract
            .timeWeightedAvgSqrtPriceX96(perp_id, 1)
            .call()
            .await?;
        let price = crate::convert::sqrt_price_x96_to_price(sqrt_price_x96)?;

        tracing::debug!(%perp_id, price, "mark price fetched");

        // Update cache
        {
            let mut cache = self.state_cache.lock().unwrap();
            cache.put_mark_price(perp_bytes, price, now_ts);
        }

        Ok(price)
    }

    /// Get the oracle index price from a beacon contract.
    ///
    /// The beacon address is available from `PerpData.beacon` (returned by
    /// [`get_perp_config`](Self::get_perp_config)).
    pub async fn get_index_price(&self, beacon: Address) -> Result<f64> {
        let contract = IBeacon::new(beacon, &self.provider);
        let index_x96: U256 = contract.index().call().await?;

        if index_x96.is_zero() {
            return Err(PerpCityError::InvalidPrice {
                reason: "beacon returned zero index".into(),
            });
        }

        crate::convert::price_x96_to_f64(index_x96)
    }

    /// Simulate closing a position to get live PnL, funding, and liquidation status.
    ///
    /// This is a read-only call (no transaction sent).
    pub async fn get_live_details(&self, pos_id: U256) -> Result<LiveDetails> {
        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let result = contract.quoteClosePosition(pos_id).call().await?;

        // Check for unexpected revert reason
        if !result.unexpectedReason.is_empty() {
            return Err(PerpCityError::TxReverted {
                reason: format!(
                    "quoteClosePosition reverted: 0x{}",
                    alloy::primitives::hex::encode(&result.unexpectedReason)
                ),
            });
        }

        let scale = SCALE_F64;
        Ok(LiveDetails {
            pnl: i128_from_i256(result.pnl) as f64 / scale,
            funding_payment: i128_from_i256(result.funding) as f64 / scale,
            effective_margin: i128_from_i256(result.netMargin) as f64 / scale,
            is_liquidatable: result.wasLiquidated,
        })
    }

    /// Get taker open interest for a perp market.
    pub async fn get_open_interest(&self, perp_id: B256) -> Result<OpenInterest> {
        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let result = contract.takerOpenInterest(perp_id).call().await?;

        let scale = SCALE_F64;
        Ok(OpenInterest {
            long_oi: result.longOI as f64 / scale,
            short_oi: result.shortOI as f64 / scale,
        })
    }

    /// Simulate opening a taker position without sending a transaction.
    ///
    /// Returns the perp and USD deltas that would result from the trade.
    /// Useful for estimating price impact before committing capital.
    pub async fn quote_open_taker(
        &self,
        perp_id: B256,
        params: &OpenTakerParams,
    ) -> Result<OpenTakerQuote> {
        let margin_scaled = scale_to_6dec(params.margin)?;
        if margin_scaled <= 0 {
            return Err(PerpCityError::InvalidMargin {
                reason: format!("margin must be positive, got {}", params.margin),
            });
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
        let result = contract
            .quoteOpenTakerPosition(perp_id, wire_params)
            .call()
            .await?;

        if !result.unexpectedReason.is_empty() {
            return Err(PerpCityError::TxReverted {
                reason: format!(
                    "quoteOpenTakerPosition reverted: 0x{}",
                    alloy::primitives::hex::encode(&result.unexpectedReason)
                ),
            });
        }

        let scale = SCALE_F64;
        Ok(OpenTakerQuote {
            perp_delta: i128_from_i256(result.perpDelta) as f64 / scale,
            usd_delta: i128_from_i256(result.usdDelta) as f64 / scale,
        })
    }

    /// Simulate opening a maker (LP) position without sending a transaction.
    ///
    /// Returns the perp and USD deltas that would result from the position.
    pub async fn quote_open_maker(
        &self,
        perp_id: B256,
        params: &OpenMakerParams,
    ) -> Result<OpenMakerQuote> {
        let margin_scaled = scale_to_6dec(params.margin)?;
        if margin_scaled <= 0 {
            return Err(PerpCityError::InvalidMargin {
                reason: format!("margin must be positive, got {}", params.margin),
            });
        }

        let tick_lower = align_tick_down(
            price_to_tick(params.price_lower)?,
            crate::constants::TICK_SPACING,
        );
        let tick_upper = align_tick_up(
            price_to_tick(params.price_upper)?,
            crate::constants::TICK_SPACING,
        );

        let wire_params = PerpManager::OpenMakerPositionParams {
            holder: self.address,
            margin: margin_scaled as u128,
            tickLower: i32_to_i24(tick_lower),
            tickUpper: i32_to_i24(tick_upper),
            liquidity: alloy::primitives::Uint::<120, 2>::from(params.liquidity),
            maxAmt0In: params.max_amt0_in,
            maxAmt1In: params.max_amt1_in,
        };

        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let result = contract
            .quoteOpenMakerPosition(perp_id, wire_params)
            .call()
            .await?;

        if !result.unexpectedReason.is_empty() {
            return Err(PerpCityError::TxReverted {
                reason: format!(
                    "quoteOpenMakerPosition reverted: 0x{}",
                    alloy::primitives::hex::encode(&result.unexpectedReason)
                ),
            });
        }

        let scale = SCALE_F64;
        Ok(OpenMakerQuote {
            perp_delta: i128_from_i256(result.perpDelta) as f64 / scale,
            usd_delta: i128_from_i256(result.usdDelta) as f64 / scale,
        })
    }

    /// Simulate a raw swap in a perp's Uniswap V4 pool without executing.
    ///
    /// This is the lowest-level quote — it simulates a single pool swap and
    /// returns the resulting token deltas. Use this to estimate price impact
    /// for a given trade size.
    ///
    /// # Arguments
    ///
    /// * `perp_id` — The perp market to quote against.
    /// * `zero_for_one` — Swap direction: `true` sells token0 for token1.
    /// * `is_exact_in` — `true` if `amount` is the exact input; `false` for exact output.
    /// * `amount` — The swap amount (scaled to 6 decimals).
    /// * `sqrt_price_limit_x96` — Price limit in sqrtPriceX96 format. Use `0` for no limit.
    pub async fn quote_swap(
        &self,
        perp_id: B256,
        zero_for_one: bool,
        is_exact_in: bool,
        amount: U256,
        sqrt_price_limit_x96: U256,
    ) -> Result<SwapQuote> {
        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let sqrt_limit = alloy::primitives::Uint::<160, 3>::from(sqrt_price_limit_x96);
        let result = contract
            .quoteSwap(perp_id, zero_for_one, is_exact_in, amount, sqrt_limit)
            .call()
            .await?;

        if !result.unexpectedReason.is_empty() {
            return Err(PerpCityError::TxReverted {
                reason: format!(
                    "quoteSwap reverted: 0x{}",
                    alloy::primitives::hex::encode(&result.unexpectedReason)
                ),
            });
        }

        let scale = SCALE_F64;
        Ok(SwapQuote {
            perp_delta: i128_from_i256(result.perpDelta) as f64 / scale,
            usd_delta: i128_from_i256(result.usdDelta) as f64 / scale,
        })
    }

    /// Get the funding rate per second for a perp, converted to a daily rate.
    ///
    /// Uses the fast cache layer (2s TTL).
    pub async fn get_funding_rate(&self, perp_id: B256) -> Result<f64> {
        let now_ts = now_secs();
        let perp_bytes: [u8; 32] = perp_id.into();

        // Check cache
        {
            let cache = self.state_cache.lock().unwrap();
            if let Some(rate) = cache.get_funding_rate(&perp_bytes, now_ts) {
                tracing::trace!(%perp_id, rate, "funding rate cache hit");
                return Ok(rate);
            }
        }

        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let funding_x96: I256 = contract.fundingPerSecondX96(perp_id).call().await?;

        // Convert from X96 fixed-point to human-readable daily rate
        // rate_per_sec = funding_x96 / 2^96
        // daily_rate = rate_per_sec * 86400
        let funding_i128 = i128_from_i256(funding_x96);
        let q96_f64 = 2.0_f64.powi(96);
        let rate_per_sec = funding_i128 as f64 / q96_f64;
        let daily_rate = rate_per_sec * crate::constants::INTERVAL as f64;

        tracing::debug!(%perp_id, daily_rate, "funding rate fetched");

        // Update cache
        {
            let mut cache = self.state_cache.lock().unwrap();
            cache.put_funding_rate(perp_bytes, daily_rate, now_ts);
        }

        Ok(daily_rate)
    }

    /// Get the USDC balance of the signer's address.
    ///
    /// Uses the fast cache layer (2s TTL).
    pub async fn get_usdc_balance(&self) -> Result<f64> {
        let now_ts = now_secs();

        // Check cache
        {
            let cache = self.state_cache.lock().unwrap();
            if let Some(bal) = cache.get_usdc_balance(now_ts) {
                tracing::trace!(balance = bal, "USDC balance cache hit");
                return Ok(bal);
            }
        }

        let usdc = IERC20::new(self.deployments.usdc, &self.provider);
        let raw: U256 = usdc.balanceOf(self.address).call().await?;
        let raw_i128 = i128::try_from(raw).map_err(|_| PerpCityError::Overflow {
            context: format!("USDC balance {} exceeds i128::MAX", raw),
        })?;
        let balance = scale_from_6dec(raw_i128);

        tracing::debug!(balance, "USDC balance fetched");

        // Update cache
        {
            let mut cache = self.state_cache.lock().unwrap();
            cache.put_usdc_balance(balance, now_ts);
        }

        Ok(balance)
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

    /// Confirm a transaction as mined. Removes from in-flight tracking.
    pub fn confirm_tx(&self, tx_hash: &[u8; 32]) {
        let mut pipeline = self.pipeline.lock().unwrap();
        pipeline.confirm(tx_hash);
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

    // ── Internal helpers ─────────────────────────────────────────────

    // ── Transfer helpers ─────────────────────────────────────────────

    /// Transfer ETH to an address. Uses the transaction pipeline for
    /// correct nonce management.
    pub async fn transfer_eth(
        &self,
        to: Address,
        amount_wei: u128,
        urgency: Urgency,
    ) -> Result<B256> {
        tracing::info!(%to, amount_wei, ?urgency, "transferring ETH");
        let receipt = self
            .send_tx_with_value(to, Bytes::new(), amount_wei, 21_000, urgency)
            .await?;
        tracing::info!(tx_hash = %receipt.transaction_hash, "ETH transferred");
        Ok(receipt.transaction_hash)
    }

    /// Transfer USDC to an address. `amount` is in human units (e.g. 100.0 = 100 USDC).
    /// Uses the transaction pipeline for correct nonce management.
    pub async fn transfer_usdc(&self, to: Address, amount: f64, urgency: Urgency) -> Result<B256> {
        tracing::info!(%to, amount, ?urgency, "transferring USDC");
        let usdc = IERC20::new(self.deployments.usdc, &self.provider);
        let scaled = U256::from(scale_to_6dec(amount)? as u128);
        let calldata = usdc.transfer(to, scaled).calldata().clone();
        let receipt = self
            .send_tx(
                self.deployments.usdc,
                calldata,
                GasLimits::TRANSFER,
                urgency,
            )
            .await?;
        tracing::info!(tx_hash = %receipt.transaction_hash, "USDC transferred");
        Ok(receipt.transaction_hash)
    }

    // ── Internal helpers ─────────────────────────────────────────────

    /// Prepare, sign, send, and wait for a transaction receipt.
    async fn send_tx(
        &self,
        to: Address,
        calldata: Bytes,
        gas_limit: u64,
        urgency: Urgency,
    ) -> Result<alloy::rpc::types::TransactionReceipt> {
        self.send_tx_with_value(to, calldata, 0, gas_limit, urgency)
            .await
    }

    /// Like `send_tx` but with an explicit ETH value to attach.
    async fn send_tx_with_value(
        &self,
        to: Address,
        calldata: Bytes,
        value: u128,
        gas_limit: u64,
        urgency: Urgency,
    ) -> Result<alloy::rpc::types::TransactionReceipt> {
        let now = now_ms();

        // Prepare via pipeline (zero RPC)
        let prepared = {
            let pipeline = self.pipeline.lock().unwrap();
            let gas_cache = self.gas_cache.lock().unwrap();
            pipeline.prepare(
                TxRequest {
                    to: to.into_array(),
                    calldata: calldata.to_vec(),
                    value,
                    gas_limit,
                    urgency,
                },
                &gas_cache,
                now,
            )?
        };

        tracing::debug!(
            nonce = prepared.nonce,
            gas_limit = prepared.gas_limit,
            max_fee = prepared.gas_fees.max_fee_per_gas,
            priority_fee = prepared.gas_fees.max_priority_fee_per_gas,
            %to,
            ?urgency,
            "tx prepared"
        );

        // Build EIP-1559 transaction
        let tx = TransactionRequest::default()
            .with_to(to)
            .with_input(calldata)
            .with_value(U256::from(prepared.request.value))
            .with_nonce(prepared.nonce)
            .with_gas_limit(prepared.gas_limit)
            .with_max_fee_per_gas(prepared.gas_fees.max_fee_per_gas as u128)
            .with_max_priority_fee_per_gas(prepared.gas_fees.max_priority_fee_per_gas as u128)
            .with_chain_id(self.chain_id);

        // Sign and send
        let tx_envelope = tx
            .build(&self.wallet)
            .await
            .map_err(|e| PerpCityError::TxReverted {
                reason: format!("failed to sign transaction: {e}"),
            })?;

        let pending = self.provider.send_tx_envelope(tx_envelope).await?;
        let tx_hash_b256 = *pending.tx_hash();
        let tx_hash_bytes: [u8; 32] = tx_hash_b256.into();

        tracing::info!(tx_hash = %tx_hash_b256, nonce = prepared.nonce, ?urgency, "tx broadcast");

        // Record in pipeline
        {
            let mut pipeline = self.pipeline.lock().unwrap();
            pipeline.record_submission(tx_hash_bytes, prepared, now);
        }

        // Wait for receipt
        let receipt = tokio::time::timeout(RECEIPT_TIMEOUT, pending.get_receipt())
            .await
            .map_err(|_| {
                tracing::warn!(tx_hash = %tx_hash_b256, timeout_secs = RECEIPT_TIMEOUT.as_secs(), "receipt timeout");
                PerpCityError::TxReverted {
                    reason: format!("receipt timeout after {}s", RECEIPT_TIMEOUT.as_secs()),
                }
            })?
            .map_err(|e| PerpCityError::TxReverted {
                reason: format!("failed to get receipt: {e}"),
            })?;

        // Confirm in pipeline
        {
            let mut pipeline = self.pipeline.lock().unwrap();
            pipeline.confirm(&tx_hash_bytes);
        }

        // Check if reverted
        if !receipt.status() {
            tracing::warn!(tx_hash = %tx_hash_b256, "tx reverted");
            return Err(PerpCityError::TxReverted {
                reason: format!("transaction {} reverted", tx_hash_b256),
            });
        }

        tracing::info!(
            tx_hash = %tx_hash_b256,
            block = ?receipt.block_number,
            gas_used = ?receipt.gas_used,
            "tx confirmed"
        );

        Ok(receipt)
    }

    /// Fetch fees from the IFees module contract.
    async fn fetch_fees(&self, config: &PerpManager::PerpConfig) -> Result<Fees> {
        if config.fees == Address::ZERO {
            return Err(PerpCityError::ModuleNotRegistered {
                module: "IFees".into(),
            });
        }

        let fees_contract = IFees::new(config.fees, &self.provider);

        let fee_result = fees_contract.fees(config.clone()).call().await?;
        let c_fee = u24_to_u32(fee_result.cFee);
        let ins_fee = u24_to_u32(fee_result.insFee);
        let lp_fee = u24_to_u32(fee_result.lpFee);

        let liq_result = fees_contract.liquidationFee(config.clone()).call().await?;
        let liq_fee = u24_to_u32(liq_result);

        let scale = SCALE_F64;
        Ok(Fees {
            creator_fee: c_fee as f64 / scale,
            insurance_fee: ins_fee as f64 / scale,
            lp_fee: lp_fee as f64 / scale,
            liquidation_fee: liq_fee as f64 / scale,
        })
    }

    /// Fetch margin ratio bounds from the IMarginRatios module contract.
    async fn fetch_bounds(&self, config: &PerpManager::PerpConfig) -> Result<Bounds> {
        if config.marginRatios == Address::ZERO {
            return Err(PerpCityError::ModuleNotRegistered {
                module: "IMarginRatios".into(),
            });
        }

        let ratios_contract = IMarginRatios::new(config.marginRatios, &self.provider);

        let ratios: IMarginRatios::MarginRatios = ratios_contract
            .marginRatios(config.clone(), false) // isMaker = false for taker bounds
            .call()
            .await?;

        let scale = SCALE_F64;
        Ok(Bounds {
            min_margin: scale_from_6dec(crate::constants::MIN_OPENING_MARGIN as i128),
            min_taker_leverage: margin_ratio_to_leverage(u24_to_u32(ratios.max))?,
            max_taker_leverage: margin_ratio_to_leverage(u24_to_u32(ratios.min))?,
            liquidation_taker_ratio: u24_to_u32(ratios.liq) as f64 / scale,
        })
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
fn parse_open_result(receipt: &alloy::rpc::types::TransactionReceipt) -> Result<OpenResult> {
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
    Err(PerpCityError::EventNotFound {
        event_name: "PositionOpened".into(),
    })
}

/// Parse an [`AdjustNotionalResult`] from a transaction receipt's `NotionalAdjusted` event.
fn parse_adjust_result(
    receipt: &alloy::rpc::types::TransactionReceipt,
) -> Result<AdjustNotionalResult> {
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
    Err(PerpCityError::EventNotFound {
        event_name: "NotionalAdjusted".into(),
    })
}

/// Parse an [`AdjustMarginResult`] from a transaction receipt's `MarginAdjusted` event.
fn parse_margin_result(
    receipt: &alloy::rpc::types::TransactionReceipt,
) -> Result<AdjustMarginResult> {
    for log in receipt.inner.logs() {
        if let Ok(event) = log.log_decode::<PerpManager::MarginAdjusted>() {
            return Ok(AdjustMarginResult {
                new_margin: u256_to_f64_6dec(event.inner.data.newMargin),
            });
        }
    }
    Err(PerpCityError::EventNotFound {
        event_name: "MarginAdjusted".into(),
    })
}

/// Parse a [`CloseResult`] from a transaction receipt's `PositionClosed` event.
fn parse_close_result(
    receipt: &alloy::rpc::types::TransactionReceipt,
    pos_id: U256,
) -> Result<CloseResult> {
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
    Err(PerpCityError::EventNotFound {
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
