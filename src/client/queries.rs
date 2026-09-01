//! Read operations: market data, positions, balances, and multicall batches.
//!
//! The client is bound to a single `Perp` market (`deployments.perp`). There is
//! no `PerpManager` and no `perp_id` — the market is identified by which `Perp`
//! contract the client points at. Positions are keyed by `posId` (ERC721 token
//! id) within that `Perp`.
//!
//! Pre-trade quoting (the old `quote_*` / `quoteClosePosition` family) is not
//! available: the frozen `Perp` exposes no on-chain quote/preview views. That
//! surface will return in a later stage (off-chain math or `eth_call`
//! simulation).

use std::collections::{BTreeMap, BTreeSet};
use std::future::IntoFuture;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{Address, B256, I256, U256};
use alloy::providers::Provider;
use alloy::sol_types::{SolCall, SolValue};
use alloy::transports::TransportError;
use futures_util::stream::{self, StreamExt};

use crate::constants::{
    MAX_SWAP_SQRT_PRICE_X96, MAX_TICK, MIN_SWAP_SQRT_PRICE_X96, MIN_TICK, MULTICALL3,
};
use crate::contracts::{
    IBeacon, IERC20, IFees, IMarginRatios, IMulticall3, IPoolManagerState, IPriceImpact, Maker,
    Perp, Position,
};
use crate::convert::{
    margin_ratio_to_leverage, price_f64_to_x96, price_x96_to_f64, scale_from_6dec,
    unpack_balance_delta,
};
use crate::errors::{ContractError, PerpCityError, Result, ValidationError};
use crate::hft::state_cache::{CachedBounds, CachedFees};
use crate::math::BlockContext;
use crate::math::ema::{PricePair, calculate_emas};
use crate::math::maker_equity::{
    AccrualInputs, MakerEquityBreakdown, MakerMarketSnapshot, MakerState, TickFunding,
    fee_growth_inside1,
};
use crate::math::storage::{
    perp_tick_slot, v4_fee_growth_global1_slot, v4_position_fee_growth_inside1_slot,
    v4_tick_bitmap_slot, v4_tick_fee_growth_outside1_slot, v4_tick_slot,
};
use crate::math::swap::{TakerMarketSnapshot, TickLiquidity};
use crate::math::tick::get_sqrt_ratio_at_tick;
use crate::types::{Bounds, Fees, OpenInterest, PerpData, PerpSnapshot};

use super::{PerpClient, SCALE_F64, i24_to_i32, now_secs, u24_to_u32};

/// Funding/utilization rates are scaled by 1e18 per day on-chain.
const WAD_F64: f64 = 1e18;

/// Perp/pool values fixed at deployment, cached after the first taker book
/// load. All are Solidity `immutable`s (or built from them), so no block
/// pinning is needed and they can never go stale.
#[derive(Debug, Clone, Copy)]
pub(super) struct BookImmutables {
    /// Uniswap V4 `PoolId` of the market's pool.
    pool_id: B256,
    /// Pool tick spacing (validated positive at load).
    tick_spacing: i32,
    /// Contract `EMA_WINDOW` decay constant, in seconds.
    ema_window: u64,
}

/// Convert an `int88` per-day funding rate (scaled by 1e18) to a human-readable
/// fraction. An 88-bit signed value always fits in `i128`.
fn funding_per_day_to_f64(rate: alloy::primitives::Signed<88, 2>) -> f64 {
    i128::try_from(rate).unwrap_or(0) as f64 / WAD_F64
}

/// Blocks to lag behind the head when pinning maker-equity reads: on
/// load-balanced RPC endpoints the newest block's state may not be
/// materialized on every replica yet, and Arbitrum produces ~4 blocks/s so
/// the lag stays under two seconds.
const MAKER_EQUITY_BLOCK_LAG: u64 = 8;

/// Concurrency bound for the `eth_getStorageAt` fallback when the endpoint
/// does not serve `eth_getProof`.
const TICK_READ_CONCURRENCY: usize = 16;

/// Outcome of one tick's funding read: the two decoded words, or the shared
/// transport error that failed every position referencing the tick.
type TickFundingRead = std::result::Result<TickFunding, std::sync::Arc<TransportError>>;

/// A maker position that survived the row-read phase and awaits slot reads.
struct PendingMaker {
    /// Index into the caller's `pos_ids`, so per-position results can be
    /// merged back into input order without positional bookkeeping.
    input_index: usize,
    pos_id: U256,
    position: Position,
    details: Maker,
    tick_lower: i32,
    tick_upper: i32,
}

/// Decode one position's `positions` + `makerDetails` multicall rows.
///
/// Returns `Ok(None)` for zero-liquidity ids (takers, deleted positions).
/// Ticks are validated against the Uniswap domain here so downstream tick
/// math cannot fail on chain-supplied values.
fn decode_maker_row(
    input_index: usize,
    pos_id: U256,
    position_row: &IMulticall3::Result,
    details_row: &IMulticall3::Result,
) -> Result<Option<PendingMaker>> {
    if !position_row.success || !details_row.success {
        return Err(ContractError::MulticallFailed {
            reason: format!("maker equity: position {pos_id} row read reverted"),
        }
        .into());
    }
    let decode_err = |context: String| ValidationError::DecodeFailed { context };
    let position = Perp::positionsCall::abi_decode_returns(&position_row.returnData)
        .map_err(|e| decode_err(format!("position {pos_id}: {e}")))?;
    let details = Perp::makerDetailsCall::abi_decode_returns(&details_row.returnData)
        .map_err(|e| decode_err(format!("makerDetails {pos_id}: {e}")))?;
    if details.liquidity == 0 {
        return Ok(None);
    }
    let tick_lower = i24_to_i32(details.tickLower);
    let tick_upper = i24_to_i32(details.tickUpper);
    get_sqrt_ratio_at_tick(tick_lower)?;
    get_sqrt_ratio_at_tick(tick_upper)?;
    Ok(Some(PendingMaker {
        input_index,
        pos_id,
        position,
        details,
        tick_lower,
        tick_upper,
    }))
}

/// Layout of the single PoolManager `extsload` batch over the V4 fee-growth
/// state for a set of maker positions: word 0 is the pool's
/// `feeGrowthGlobal1X128`, followed by one `feeGrowthOutside1X128` word per
/// DISTINCT band tick (positions in a maker ladder share boundaries, so a
/// shared tick is read once), then one `feeGrowthInside1LastX128` word per
/// position.
struct FeeGrowthLayout {
    slots: Vec<B256>,
    /// Word index of each distinct tick's outside-growth slot.
    tick_word: BTreeMap<i32, usize>,
    /// Word index of the first per-position inside-growth slot.
    first_inside_word: usize,
}

impl FeeGrowthLayout {
    fn new(pool_id: B256, perp: Address, pending: &[PendingMaker]) -> Self {
        let ticks: BTreeSet<i32> = pending
            .iter()
            .flat_map(|maker| [maker.tick_lower, maker.tick_upper])
            .collect();
        let mut slots = Vec::with_capacity(1 + ticks.len() + pending.len());
        slots.push(B256::from(v4_fee_growth_global1_slot(pool_id)));
        let mut tick_word = BTreeMap::new();
        for tick in ticks {
            tick_word.insert(tick, slots.len());
            slots.push(B256::from(v4_tick_fee_growth_outside1_slot(pool_id, tick)));
        }
        let first_inside_word = slots.len();
        for maker in pending {
            slots.push(B256::from(v4_position_fee_growth_inside1_slot(
                pool_id,
                perp,
                maker.tick_lower,
                maker.tick_upper,
                B256::from(maker.pos_id),
            )));
        }
        Self {
            slots,
            tick_word,
            first_inside_word,
        }
    }

    fn word_count(&self) -> usize {
        self.slots.len()
    }

    fn global(&self, words: &[B256]) -> U256 {
        U256::from_be_bytes(words[0].0)
    }

    /// `feeGrowthOutside1X128` of `tick`. The tick must be a band boundary
    /// of one of the positions the layout was built from.
    fn outside(&self, words: &[B256], tick: i32) -> U256 {
        U256::from_be_bytes(words[self.tick_word[&tick]].0)
    }

    /// `feeGrowthInside1LastX128` of the `position`-th pending position.
    fn inside_last(&self, words: &[B256], position: usize) -> U256 {
        U256::from_be_bytes(words[self.first_inside_word + position].0)
    }
}

/// Resolve one band tick's funding read for a position. A failed (or
/// absent) tick read converts into the typed storage error handed to every
/// position referencing that tick — and only those positions.
fn tick_funding_for(funding: &BTreeMap<i32, TickFundingRead>, tick: i32) -> Result<TickFunding> {
    match funding.get(&tick) {
        Some(Ok(funding)) => Ok(*funding),
        Some(Err(e)) => Err(ContractError::StorageReadFailed {
            context: format!("tick {tick} funding"),
            source: Some(std::sync::Arc::clone(e)),
        }
        .into()),
        None => Err(ContractError::StorageReadFailed {
            context: format!("tick {tick} funding missing from batch"),
            source: None,
        }
        .into()),
    }
}

/// Whether a transport error means the RPC method itself is not available
/// on the endpoint, as opposed to the call failing.
fn method_unsupported(e: &alloy::transports::TransportError) -> bool {
    const METHOD_NOT_FOUND: i64 = -32601;
    e.as_error_resp().is_some_and(|resp| {
        resp.code == METHOD_NOT_FOUND || {
            let message = resp.message.to_lowercase();
            message.contains("not supported")
                || message.contains("unsupported")
                || message.contains("method not found")
                || message.contains("does not exist")
        }
    })
}

/// Split the row-multicall results into positions awaiting slot reads and
/// per-position failures, both tagged with their input index. Zero-liquidity
/// ids (takers, deleted positions) are dropped. A failed row degrades only
/// its own position; the rest of the batch survives.
fn split_maker_rows(
    pos_ids: &[U256],
    rows: &[IMulticall3::Result],
) -> (Vec<PendingMaker>, Vec<(usize, U256, PerpCityError)>) {
    let mut pending = Vec::new();
    let mut failed = Vec::new();
    for (input_index, (row, &pos_id)) in rows.chunks_exact(2).zip(pos_ids).enumerate() {
        match decode_maker_row(input_index, pos_id, &row[0], &row[1]) {
            Ok(None) => {}
            Ok(Some(maker)) => pending.push(maker),
            Err(e) => {
                tracing::warn!(%pos_id, error = %e, "maker equity: position row failed");
                failed.push((input_index, pos_id, e));
            }
        }
    }
    (pending, failed)
}

impl PerpClient {
    // ── Read operations ──────────────────────────────────────────────

    /// Cache key for this client's market: the `Perp` address left-padded to 32 bytes.
    fn market_key(&self) -> [u8; 32] {
        self.deployments.perp.into_word().0
    }

    /// Fetch and cache the deployment-fixed values the taker book loader
    /// needs. The first call costs three RPC reads; every later call is free.
    async fn book_immutables(&self) -> Result<&BookImmutables> {
        self.book_immutables
            .get_or_try_init(|| async {
                let perp = Perp::new(self.deployments.perp, &self.provider);
                let pool_id_call = perp.POOL_ID();
                let pool_key_call = perp.poolKey();
                let ema_window_call = perp.EMA_WINDOW();
                let (pool_id, pool_key, ema_window) = tokio::try_join!(
                    pool_id_call.call(),
                    pool_key_call.call(),
                    ema_window_call.call(),
                )?;
                let tick_spacing = i24_to_i32(pool_key.tickSpacing);
                if tick_spacing <= 0 {
                    return Err(ValidationError::InvalidConfig {
                        reason: format!("invalid tick spacing {tick_spacing}"),
                    }
                    .into());
                }
                if ema_window > U256::from(u64::MAX) {
                    return Err(ValidationError::Overflow {
                        context: "EMA window".into(),
                    }
                    .into());
                }
                Ok(BookImmutables {
                    pool_id,
                    tick_spacing,
                    ema_window: ema_window.to::<u64>(),
                })
            })
            .await
    }

    /// Load an exact concentrated-liquidity snapshot at the latest canonical
    /// block for the deployed Perp contract (`perpcity-contracts@4bbe554f`).
    ///
    /// The loader reads the stored EMA pair from its deployed storage slot,
    /// advances it with the contract's exact arithmetic, and evaluates the
    /// configured price-impact module. Every contract and PoolManager read is
    /// pinned to the returned block hash.
    pub async fn load_taker_market_snapshot(&self) -> Result<TakerMarketSnapshot> {
        let immutables = *self.book_immutables().await?;
        let block = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await?
            .ok_or_else(|| ContractError::MulticallFailed {
                reason: "latest block not found".into(),
            })?;
        let block_id = BlockId::hash(block.header.hash);
        let perp = Perp::new(self.deployments.perp, &self.provider);
        // PerpStorage starts at slot 3 and `emas` is field slot 8 in the
        // deployed layout. PricePair packs ammPrice in the low 128 bits and
        // index in the high 128 bits.
        const DEPLOYED_EMAS_SLOT: u64 = 11;
        let stored_emas = self
            .provider
            .get_storage_at(self.deployments.perp, U256::from(DEPLOYED_EMAS_SLOT))
            .block_id(block_id)
            .await?;
        let pool_state_call = perp.poolState().block(block_id);
        let modules_call = perp.modules().block(block_id);
        let rates_call = perp.rates().block(block_id);
        let (state, modules, rates) = tokio::try_join!(
            pool_state_call.call(),
            modules_call.call(),
            rates_call.call(),
        )?;
        let index = IBeacon::new(modules.beacon, &self.provider)
            .index()
            .block(block_id)
            .call()
            .await?;
        if state.ammPrice > U256::from(u128::MAX) || index > U256::from(u128::MAX) {
            return Err(ValidationError::Overflow {
                context: "deployed EMA inputs".into(),
            }
            .into());
        }
        let stored = PricePair {
            amm: (stored_emas & U256::from(u128::MAX)).to::<u128>(),
            index: (stored_emas >> 128usize).to::<u128>(),
        };
        let spot = PricePair {
            amm: state.ammPrice.to::<u128>(),
            index: index.to::<u128>(),
        };
        let emas = calculate_emas(
            stored,
            spot,
            rates.lastTouch.to::<u64>(),
            block.header.timestamp,
            immutables.ema_window,
        )?;
        let bounds = IPriceImpact::new(modules.priceImpact, &self.provider)
            .sqrtPriceBounds(
                state.ammPrice,
                index,
                U256::from(emas.amm),
                U256::from(emas.index),
            )
            .block(block_id)
            .call()
            .await?;
        let header = TakerMarketSnapshot {
            block: BlockContext {
                number: block.header.number,
                hash: block.header.hash,
                timestamp: block.header.timestamp,
            },
            sqrt_price_x96: state.sqrtPrice.to::<U256>(),
            tick: i24_to_i32(state.tick),
            liquidity: state.liquidity,
            ticks: BTreeMap::new(),
            protocol_sqrt_min_x96: MIN_SWAP_SQRT_PRICE_X96,
            protocol_sqrt_max_x96: MAX_SWAP_SQRT_PRICE_X96,
            impact_sqrt_min_x96: bounds.sqrtMin,
            impact_sqrt_max_x96: bounds.sqrtMax,
        };
        self.fill_book(header).await
    }

    /// Populate `snapshot.ticks` from the PoolManager's tick bitmap at the
    /// snapshot's block, then verify the reconstruction against the pool's
    /// reported active liquidity before returning it.
    async fn fill_book(&self, mut snapshot: TakerMarketSnapshot) -> Result<TakerMarketSnapshot> {
        let block_id = BlockId::hash(snapshot.block.hash);
        let BookImmutables {
            pool_id,
            tick_spacing: spacing,
            ..
        } = *self.book_immutables().await?;

        let min_word = MIN_TICK.div_euclid(spacing).div_euclid(256);
        let max_word = MAX_TICK.div_euclid(spacing).div_euclid(256);
        let bitmap_slots: Vec<B256> = (min_word..=max_word)
            .map(|word| B256::from(v4_tick_bitmap_slot(pool_id, word)))
            .collect();
        let manager = IPoolManagerState::new(self.deployments.pool_manager, &self.provider);
        let bitmaps = manager
            .extsload_1(bitmap_slots)
            .block(block_id)
            .call()
            .await?;

        let mut initialized = Vec::new();
        for (offset, bitmap) in bitmaps.into_iter().enumerate() {
            let word = min_word + offset as i32;
            let bits = U256::from_be_bytes(bitmap.0);
            for bit in 0..256i32 {
                if bits.bit(bit as usize) {
                    let compressed = word * 256 + bit;
                    let initialized_tick = compressed * spacing;
                    if (MIN_TICK..=MAX_TICK).contains(&initialized_tick) {
                        initialized.push(initialized_tick);
                    }
                }
            }
        }

        let tick_slots: Vec<B256> = initialized
            .iter()
            .map(|&initialized_tick| B256::from(v4_tick_slot(pool_id, initialized_tick)))
            .collect();
        let tick_words = if tick_slots.is_empty() {
            Vec::new()
        } else {
            manager
                .extsload_1(tick_slots)
                .block(block_id)
                .call()
                .await?
        };
        let mut ticks = BTreeMap::new();
        for (initialized_tick, word) in initialized.into_iter().zip(tick_words) {
            let raw = U256::from_be_bytes(word.0);
            let gross = (raw & U256::from(u128::MAX)).to::<u128>();
            let net_raw = (raw >> 128usize).to::<u128>();
            let net = net_raw as i128;
            ticks.insert(initialized_tick, TickLiquidity { gross, net });
        }

        let reconstructed = ticks
            .range(..=snapshot.tick)
            .try_fold(0u128, |active, (_, info)| {
                if info.net >= 0 {
                    active.checked_add(info.net as u128)
                } else {
                    active.checked_sub(info.net.unsigned_abs())
                }
            })
            .ok_or_else(|| ValidationError::Overflow {
                context: "reconstructing active liquidity".into(),
            })?;
        if reconstructed != snapshot.liquidity {
            return Err(ContractError::MulticallFailed {
                reason: format!(
                    "tick snapshot liquidity mismatch: reconstructed {reconstructed}, pool {}",
                    snapshot.liquidity
                ),
            }
            .into());
        }

        snapshot.ticks = ticks;
        Ok(snapshot)
    }

    /// Get the full perp configuration, fees, and bounds for the market.
    ///
    /// Uses the [`crate::hft::state_cache::StateCache`] for fees and bounds (60s TTL).
    pub async fn get_perp_config(&self) -> Result<PerpData> {
        let perp = Perp::new(self.deployments.perp, &self.provider);

        let modules = perp.modules().call().await?;
        let pool_key = perp.poolKey().call().await?;
        let pool_state = perp.poolState().call().await?;
        let mark = price_x96_to_f64(pool_state.ammPrice)?;

        let fees = self.get_or_fetch_fees(modules.fees).await?;
        let bounds = self.get_or_fetch_bounds(modules.marginRatios).await?;

        Ok(PerpData {
            perp: self.deployments.perp,
            tick_spacing: i24_to_i32(pool_key.tickSpacing),
            mark,
            beacon: modules.beacon,
            bounds,
            fees,
        })
    }

    /// Get perp data: beacon, tick spacing, and current mark price.
    ///
    /// Lighter-weight than [`Self::get_perp_config`] — skips fees/bounds lookups.
    pub async fn get_perp_data(&self) -> Result<(Address, i32, f64)> {
        let perp = Perp::new(self.deployments.perp, &self.provider);
        let modules = perp.modules().call().await?;
        let pool_key = perp.poolKey().call().await?;
        let pool_state = perp.poolState().call().await?;
        let mark = price_x96_to_f64(pool_state.ammPrice)?;

        Ok((modules.beacon, i24_to_i32(pool_key.tickSpacing), mark))
    }

    /// Get an on-chain position by its NFT token ID.
    ///
    /// Returns the raw contract position struct. Use [`crate::math::position`]
    /// functions to compute derived values (entry price, PnL, etc.).
    pub async fn get_position(&self, pos_id: U256) -> Result<Position> {
        let perp = Perp::new(self.deployments.perp, &self.provider);
        let pos = perp.positions(pos_id).call().await?;

        // A non-existent or burned position decodes to an all-zero struct.
        if pos.margin == 0 && pos.delta.is_zero() {
            return Err(ContractError::PositionNotFound { pos_id }.into());
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
        let perp = Perp::new(self.deployments.perp, &self.provider);
        let next_pos_id: U256 = perp.nextPosId().call().await?;

        let total: u64 = next_pos_id
            .try_into()
            .map_err(|_| ValidationError::Overflow {
                context: "nextPosId exceeds u64".into(),
            })?;
        if total <= 1 {
            return Ok(vec![]);
        }

        let mut owned = Vec::new();
        for id in 1..total {
            let pos_id = U256::from(id);
            // ownerOf reverts for burned/non-existent tokens — skip those.
            // How a revert surfaces is provider-dependent: some decode into
            // contract errors, others wrap the raw JSON-RPC error response
            // (code 3, revert data attached) as a transport error. Classify
            // by revert data, and only propagate genuine transport failures
            // so network errors aren't silently ignored.
            match perp.ownerOf(pos_id).call().await {
                Ok(addr) if addr == owner => owned.push(pos_id),
                Ok(_) => {}
                Err(alloy::contract::Error::TransportError(e))
                    if e.as_error_resp()
                        .and_then(|resp| resp.as_revert_data())
                        .is_none() =>
                {
                    return Err(alloy::contract::Error::TransportError(e).into());
                }
                Err(_) => {} // burned or non-existent token
            }
        }

        Ok(owned)
    }

    /// Get the current mark price for the market (AMM spot price).
    ///
    /// Reads the live Uniswap V4 pool price via `poolState`. Uses the fast
    /// cache layer (2s TTL).
    pub async fn get_mark_price(&self) -> Result<f64> {
        let now_ts = now_secs();
        let key = self.market_key();

        // Check cache
        {
            let cache = self.state_cache.lock().unwrap();
            if let Some(price) = cache.get_mark_price(&key, now_ts) {
                tracing::trace!(price, "mark price cache hit");
                return Ok(price);
            }
        }

        // Fetch from chain
        let perp = Perp::new(self.deployments.perp, &self.provider);
        let pool_state = perp.poolState().call().await?;
        let price = price_x96_to_f64(pool_state.ammPrice)?;

        tracing::debug!(price, "mark price fetched");

        // Update cache
        {
            let mut cache = self.state_cache.lock().unwrap();
            cache.put_mark_price(key, price, now_ts);
        }

        Ok(price)
    }

    /// Get the oracle index price from a beacon contract.
    ///
    /// The beacon address is available from `PerpData.beacon` (returned by
    /// [`get_perp_config`](Self::get_perp_config)).
    ///
    /// Note: `index()` is a state-mutating function on-chain; this performs an
    /// `eth_call` (simulation) and does not send a transaction.
    pub async fn get_index_price(&self, beacon: Address) -> Result<f64> {
        let contract = IBeacon::new(beacon, &self.provider);
        let index_x96: U256 = contract.index().call().await?;

        if index_x96.is_zero() {
            return Err(ValidationError::InvalidPrice {
                reason: "beacon returned zero index".into(),
            }
            .into());
        }

        let index = price_x96_to_f64(index_x96)?;
        Ok(index)
    }

    /// Get taker open interest for the market.
    pub async fn get_open_interest(&self) -> Result<OpenInterest> {
        let perp = Perp::new(self.deployments.perp, &self.provider);
        let oi = perp.openInterest().call().await?;

        Ok(OpenInterest {
            long_oi: oi.long as f64 / SCALE_F64,
            short_oi: oi.short as f64 / SCALE_F64,
        })
    }

    /// Get the current daily funding rate for the market.
    ///
    /// Reads `rates().fundingPerDay` (scaled by 1e18 per day). Positive means
    /// long-exposed positions pay short-exposed positions. Uses the fast cache
    /// layer (2s TTL).
    pub async fn get_funding_rate(&self) -> Result<f64> {
        let now_ts = now_secs();
        let key = self.market_key();

        // Check cache
        {
            let cache = self.state_cache.lock().unwrap();
            if let Some(rate) = cache.get_funding_rate(&key, now_ts) {
                tracing::trace!(rate, "funding rate cache hit");
                return Ok(rate);
            }
        }

        let perp = Perp::new(self.deployments.perp, &self.provider);
        let rates = perp.rates().call().await?;
        let daily_rate = funding_per_day_to_f64(rates.fundingPerDay);

        tracing::debug!(daily_rate, "funding rate fetched");

        // Update cache
        {
            let mut cache = self.state_cache.lock().unwrap();
            cache.put_funding_rate(key, daily_rate, now_ts);
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
        let raw_i128 = i128::try_from(raw).map_err(|_| ValidationError::Overflow {
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

    // ── Batch reads (via Multicall3) ──────────────────────────────────

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

        let usdc_addr = self.deployments.usdc;
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

        let multicall = IMulticall3::new(MULTICALL3, &self.provider);
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
            let usdc_i128 = i128::try_from(usdc_raw).map_err(|_| ValidationError::Overflow {
                context: format!("USDC balance {} exceeds i128::MAX", usdc_raw),
            })?;
            let usdc = scale_from_6dec(usdc_i128);

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

    /// Get perp config and live market data in a single multicall (plus the
    /// beacon index read).
    ///
    /// Batches `modules` + `poolKey` + `poolState` + `rates` + `openInterest`
    /// against the `Perp` contract (5 reads → 1 CU), then calls `index()` on the
    /// beacon returned by the batch (1 CU). Replaces a startup sequence of
    /// several individual RPCs.
    ///
    /// Returns `(PerpData, PerpSnapshot)` — static config and live market data.
    pub async fn get_perp_snapshot(&self) -> Result<(PerpData, PerpSnapshot)> {
        let perp_addr = self.deployments.perp;

        let calls = vec![
            IMulticall3::Call3 {
                target: perp_addr,
                allowFailure: false,
                callData: Perp::modulesCall {}.abi_encode().into(),
            },
            IMulticall3::Call3 {
                target: perp_addr,
                allowFailure: false,
                callData: Perp::poolKeyCall {}.abi_encode().into(),
            },
            IMulticall3::Call3 {
                target: perp_addr,
                allowFailure: false,
                callData: Perp::poolStateCall {}.abi_encode().into(),
            },
            IMulticall3::Call3 {
                target: perp_addr,
                allowFailure: false,
                callData: Perp::ratesCall {}.abi_encode().into(),
            },
            IMulticall3::Call3 {
                target: perp_addr,
                allowFailure: false,
                callData: Perp::openInterestCall {}.abi_encode().into(),
            },
        ];

        let multicall = IMulticall3::new(MULTICALL3, &self.provider);
        let results = multicall.aggregate3(calls).call().await?;

        let call_names = ["modules", "poolKey", "poolState", "rates", "openInterest"];
        if results.len() != call_names.len() {
            return Err(ContractError::MulticallFailed {
                reason: format!(
                    "perp snapshot multicall returned {} results, expected {}",
                    results.len(),
                    call_names.len()
                ),
            }
            .into());
        }
        for (i, name) in call_names.iter().enumerate() {
            if !results[i].success {
                return Err(ContractError::MulticallFailed {
                    reason: format!("perp snapshot multicall: {name} call failed"),
                }
                .into());
            }
        }

        let decode_err = |name: &str, e: alloy::sol_types::Error| ValidationError::DecodeFailed {
            context: format!("failed to decode {name}: {e}"),
        };

        let modules = Perp::modulesCall::abi_decode_returns(&results[0].returnData)
            .map_err(|e| decode_err("modules", e))?;
        let pool_key = Perp::poolKeyCall::abi_decode_returns(&results[1].returnData)
            .map_err(|e| decode_err("poolKey", e))?;
        let pool_state = Perp::poolStateCall::abi_decode_returns(&results[2].returnData)
            .map_err(|e| decode_err("poolState", e))?;
        let rates = Perp::ratesCall::abi_decode_returns(&results[3].returnData)
            .map_err(|e| decode_err("rates", e))?;
        let oi = Perp::openInterestCall::abi_decode_returns(&results[4].returnData)
            .map_err(|e| decode_err("openInterest", e))?;

        let mark = price_x96_to_f64(pool_state.ammPrice)?;
        let funding_rate_daily = funding_per_day_to_f64(rates.fundingPerDay);
        let open_interest = OpenInterest {
            long_oi: oi.long as f64 / SCALE_F64,
            short_oi: oi.short as f64 / SCALE_F64,
        };

        // Index price from the beacon (1 CU).
        let index_price = self.get_index_price(modules.beacon).await?;

        // Fees/bounds (from cache or chain).
        let fees = self.get_or_fetch_fees(modules.fees).await?;
        let bounds = self.get_or_fetch_bounds(modules.marginRatios).await?;

        let perp_data = PerpData {
            perp: perp_addr,
            tick_spacing: i24_to_i32(pool_key.tickSpacing),
            mark,
            beacon: modules.beacon,
            bounds,
            fees,
        };

        let snapshot = PerpSnapshot {
            mark_price: mark,
            index_price,
            funding_rate_daily,
            open_interest,
        };

        tracing::debug!("perp snapshot fetched via multicall");
        Ok((perp_data, snapshot))
    }

    // ── Maker equity (block-pinned batch read) ──────────────────────

    /// Read chain state and compute the settle-preview equity for each maker
    /// position in `pos_ids`, all pinned to one block.
    ///
    /// Reads are batched: one Multicall3 round trip for the market-wide
    /// state, one for all position/maker rows, one PoolManager `extsload`
    /// for the V4 fee-growth slots (distinct band ticks read once), and one
    /// `eth_getProof` for the distinct Perp tick-funding slots (with an
    /// `eth_getStorageAt` fallback on endpoints without `eth_getProof`).
    ///
    /// Failures degrade per position: a position whose reads, decoding, or
    /// math fail is returned with an `Err` (and logged) instead of
    /// discarding the batch. Market-wide read failures still fail the whole
    /// call. Non-maker ids (zero liquidity — takers or deleted positions)
    /// are omitted from the output.
    ///
    /// Cost grows linearly with `pos_ids`: two multicall subcalls, one
    /// extsload word, and up to two tick-funding slots per position. All
    /// four round trips comfortably carry a few hundred positions; above
    /// roughly 500 ids, chunk the calls (each chunk still pins its own
    /// block) to stay inside RPC response-size and calldata limits.
    ///
    /// `mark_price` is the caller's current mark (snapshot / market-data
    /// cache); it prices `valPnl` and the accrual replay. It must be a
    /// positive finite number.
    pub async fn read_maker_equities(
        &self,
        pos_ids: &[U256],
        mark_price: f64,
    ) -> Result<Vec<(U256, Result<MakerEquityBreakdown>)>> {
        let mark_price_x96 = price_f64_to_x96(mark_price)?;
        if pos_ids.is_empty() {
            return Ok(Vec::new());
        }
        let perp_addr = self.deployments.perp;
        let (market, pool_id, block_id) = self.load_maker_market_snapshot(mark_price_x96).await?;

        // ── Position rows: one multicall, degrading per position ────
        let row_call = |calldata: Vec<u8>| IMulticall3::Call3 {
            target: perp_addr,
            allowFailure: true,
            callData: calldata.into(),
        };
        let calls = pos_ids
            .iter()
            .flat_map(|&pos_id| {
                [
                    row_call(Perp::positionsCall { posId: pos_id }.abi_encode()),
                    row_call(Perp::makerDetailsCall { posId: pos_id }.abi_encode()),
                ]
            })
            .collect();
        let multicall = IMulticall3::new(MULTICALL3, &self.provider);
        let rows = multicall.aggregate3(calls).block(block_id).call().await?;
        if rows.len() != 2 * pos_ids.len() {
            return Err(ContractError::MulticallFailed {
                reason: format!(
                    "maker equity row multicall returned {} results, expected {}",
                    rows.len(),
                    2 * pos_ids.len()
                ),
            }
            .into());
        }

        let (pending, failed) = split_maker_rows(pos_ids, &rows);

        // Merge by input index: `read_pending_maker_equities` returns one
        // result per pending position (it maps over `pending`), so the zip
        // is structurally exhaustive — no positional counter to get wrong.
        let equities = self
            .read_pending_maker_equities(&market, pool_id, block_id, &pending)
            .await?;
        let mut out: Vec<(usize, U256, Result<MakerEquityBreakdown>)> = pending
            .iter()
            .zip(equities)
            .map(|(maker, equity)| (maker.input_index, maker.pos_id, equity))
            .collect();
        out.extend(
            failed
                .into_iter()
                .map(|(input_index, pos_id, e)| (input_index, pos_id, Err(e))),
        );
        out.sort_by_key(|&(input_index, ..)| input_index);
        Ok(out
            .into_iter()
            .map(|(_, pos_id, equity)| (pos_id, equity))
            .collect())
    }

    /// Resolve the safe lagged block and load + accrue the market-wide
    /// snapshot for [`Self::read_maker_equities`] in one multicall.
    ///
    /// Returns the accrued snapshot, the pool id, and the block id every
    /// later read must pin to.
    async fn load_maker_market_snapshot(
        &self,
        mark_price_x96: U256,
    ) -> Result<(MakerMarketSnapshot, B256, BlockId)> {
        let perp_addr = self.deployments.perp;
        let block_number = self
            .provider
            .get_block_number()
            .await?
            .saturating_sub(MAKER_EQUITY_BLOCK_LAG);
        // A lagging replica may briefly miss the pinned header. That is a
        // failed read, not a degraded one: pinning by bare number would drop
        // the reorg protection, and substituting the local wall clock for
        // the block timestamp would silently skew the accrual replay (a
        // slow clock reads as zero accrual). A settlement preview must not
        // quietly degrade — fail and let the caller retry.
        let block = self
            .provider
            .get_block_by_number(block_number.into())
            .await?
            .ok_or(ContractError::BlockUnavailable {
                number: block_number,
            })?;
        let block_id = BlockId::hash(block.header.hash);
        let (block_hash, block_timestamp) = (block.header.hash, block.header.timestamp);

        let market_call = |calldata: Vec<u8>| IMulticall3::Call3 {
            target: perp_addr,
            allowFailure: false,
            callData: calldata.into(),
        };

        // ── Position rows: one multicall, degrading per position ────
        let calls = vec![
            market_call(Perp::cumulativesCall {}.abi_encode()),
            market_call(Perp::ratesCall {}.abi_encode()),
            market_call(Perp::poolStateCall {}.abi_encode()),
            market_call(Perp::capacityCall {}.abi_encode()),
            market_call(Perp::openInterestCall {}.abi_encode()),
            market_call(Perp::POOL_IDCall {}.abi_encode()),
        ];
        let multicall = IMulticall3::new(MULTICALL3, &self.provider);
        let results = multicall.aggregate3(calls).block(block_id).call().await?;
        let call_names = [
            "cumulatives",
            "rates",
            "poolState",
            "capacity",
            "openInterest",
            "POOL_ID",
        ];
        if results.len() != call_names.len() {
            return Err(ContractError::MulticallFailed {
                reason: format!(
                    "maker equity market multicall returned {} results, expected {}",
                    results.len(),
                    call_names.len()
                ),
            }
            .into());
        }
        for (result, name) in results.iter().zip(call_names) {
            if !result.success {
                return Err(ContractError::MulticallFailed {
                    reason: format!("maker equity market multicall: {name} call failed"),
                }
                .into());
            }
        }
        let decode_err = |name: &str, e: alloy::sol_types::Error| ValidationError::DecodeFailed {
            context: format!("failed to decode {name}: {e}"),
        };
        let cumls = Perp::cumulativesCall::abi_decode_returns(&results[0].returnData)
            .map_err(|e| decode_err("cumulatives", e))?;
        let rates = Perp::ratesCall::abi_decode_returns(&results[1].returnData)
            .map_err(|e| decode_err("rates", e))?;
        let pool_state = Perp::poolStateCall::abi_decode_returns(&results[2].returnData)
            .map_err(|e| decode_err("poolState", e))?;
        let capacity = Perp::capacityCall::abi_decode_returns(&results[3].returnData)
            .map_err(|e| decode_err("capacity", e))?;
        let oi = Perp::openInterestCall::abi_decode_returns(&results[4].returnData)
            .map_err(|e| decode_err("openInterest", e))?;
        let pool_id = Perp::POOL_IDCall::abi_decode_returns(&results[5].returnData)
            .map_err(|e| decode_err("POOL_ID", e))?;

        let market = MakerMarketSnapshot {
            block: BlockContext {
                number: block_number,
                hash: block_hash,
                timestamp: block_timestamp,
            },
            funding_x96: cumls.fundingX96,
            funding_div_sqrt_p_x96: cumls.fundingDivSqrtPX96,
            long_util_earnings_x96: cumls.longUtilEarningsX96,
            short_util_earnings_x96: cumls.shortUtilEarningsX96,
            tick: i24_to_i32(pool_state.tick),
            sqrt_price_x96: pool_state.sqrtPrice.to::<U256>(),
            mark_price_x96,
        }
        .accrued(&AccrualInputs {
            funding_per_day_wad: i128::try_from(rates.fundingPerDay).unwrap_or(0),
            long_util_fee_per_day_wad: rates.longUtilFeePerDay,
            short_util_fee_per_day_wad: rates.shortUtilFeePerDay,
            last_touch: rates.lastTouch.to::<u64>(),
            now: block_timestamp,
            oi_long: oi.long,
            oi_short: oi.short,
            cap_long: capacity.long,
            cap_short: capacity.short,
        })?;
        Ok((market, pool_id, block_id))
    }

    /// Slot reads and math for the surviving positions of
    /// [`Self::read_maker_equities`]: one `extsload` batch for the V4
    /// fee-growth slots (distinct band ticks read once), one `eth_getProof`
    /// batch for the Perp tick-funding slots (also deduplicated), degrading
    /// per position on failure.
    async fn read_pending_maker_equities(
        &self,
        market: &MakerMarketSnapshot,
        pool_id: B256,
        block_id: BlockId,
        pending: &[PendingMaker],
    ) -> Result<Vec<Result<MakerEquityBreakdown>>> {
        if pending.is_empty() {
            return Ok(Vec::new());
        }

        let layout = FeeGrowthLayout::new(pool_id, self.deployments.perp, pending);
        let manager = IPoolManagerState::new(self.deployments.pool_manager, &self.provider);
        let words = manager
            .extsload_1(layout.slots.clone())
            .block(block_id)
            .call()
            .await?;
        if words.len() != layout.word_count() {
            return Err(ContractError::StorageReadFailed {
                context: format!(
                    "maker equity extsload returned {} words, expected {}",
                    words.len(),
                    layout.word_count()
                ),
                source: None,
            }
            .into());
        }
        let fg1_global = layout.global(&words);

        // Positions share band boundaries (a maker ladder reuses each inner
        // tick twice), so read each distinct tick's two funding words once.
        let ticks: BTreeSet<i32> = pending
            .iter()
            .flat_map(|maker| [maker.tick_lower, maker.tick_upper])
            .collect();
        let tick_funding = self.read_tick_funding(block_id, &ticks).await?;
        let funding_for = |tick: i32| tick_funding_for(&tick_funding, tick);

        let equities = pending
            .iter()
            .enumerate()
            .map(|(i, maker)| {
                let tick_lower_funding = funding_for(maker.tick_lower)?;
                let tick_upper_funding = funding_for(maker.tick_upper)?;
                let fg1_out_lower = layout.outside(&words, maker.tick_lower);
                let fg1_out_upper = layout.outside(&words, maker.tick_upper);
                let fg1_inside_last = layout.inside_last(&words, i);
                let (delta_amount0, delta_amount1) = unpack_balance_delta(maker.position.delta);
                let state = MakerState {
                    margin_6dec: maker.position.margin,
                    delta_amount0,
                    delta_amount1,
                    last_cuml_funding_x96: maker.position.lastCumlFundingX96,
                    tick_lower: maker.tick_lower,
                    tick_upper: maker.tick_upper,
                    liquidity: maker.details.liquidity,
                    last_long_util_earnings_x96: maker.details.lastLongUtilEarningsX96,
                    last_short_util_earnings_x96: maker.details.lastShortUtilEarningsX96,
                    cap_long_6dec: maker.details.capacity.long,
                    cap_short_6dec: maker.details.capacity.short,
                    last_below_x96: maker.details.lastCumlFunding.belowX96,
                    last_within_x96: maker.details.lastCumlFunding.withinX96,
                    last_div_sqrt_within_x96: maker.details.lastCumlFunding.divSqrtPriceWithinX96,
                    tick_lower_funding,
                    tick_upper_funding,
                    fee_growth_inside1_x128: fee_growth_inside1(
                        fg1_global,
                        fg1_out_lower,
                        fg1_out_upper,
                        maker.tick_lower,
                        maker.tick_upper,
                        market.tick,
                    ),
                    fee_growth_inside1_last_x128: fg1_inside_last,
                };
                market.maker_equity(&state).map_err(|e| {
                    tracing::warn!(
                        pos_id = %maker.pos_id, error = %e,
                        "maker equity: settle math failed"
                    );
                    e.into()
                })
            })
            .collect();
        Ok(equities)
    }

    /// Read each distinct tick's two funding words from the Perp contract,
    /// pinned to `block_id`.
    ///
    /// Primary path: one `eth_getProof` request over all slots —
    /// `storageProof[i].value` carries the storage words, the request takes
    /// a block id so the batch stays pinned, and Arbitrum Nitro serves it.
    /// An endpoint that does not support `eth_getProof` is remembered (the
    /// probe is not repeated) and the read falls back to concurrent
    /// `eth_getStorageAt` with bounded concurrency, where a failed tick
    /// read degrades only the positions referencing that tick.
    async fn read_tick_funding(
        &self,
        block_id: BlockId,
        ticks: &BTreeSet<i32>,
    ) -> Result<BTreeMap<i32, TickFundingRead>> {
        use std::sync::Arc;
        use std::sync::atomic::Ordering;

        let perp_addr = self.deployments.perp;
        if !self.get_proof_unsupported.load(Ordering::Relaxed) {
            let keys: Vec<B256> = ticks
                .iter()
                .flat_map(|&tick| {
                    let slot = perp_tick_slot(tick);
                    [B256::from(slot), B256::from(slot + U256::ONE)]
                })
                .collect();
            match self
                .provider
                .get_proof(perp_addr, keys)
                .block_id(block_id)
                .await
            {
                Ok(proof) => {
                    let values: BTreeMap<B256, U256> = proof
                        .storage_proof
                        .iter()
                        .map(|entry| (entry.key.as_b256(), entry.value))
                        .collect();
                    let mut funding = BTreeMap::new();
                    for &tick in ticks {
                        let slot = perp_tick_slot(tick);
                        let word = |slot: U256| values.get(&B256::from(slot)).copied();
                        let (Some(opp), Some(div_sqrt_p_opp)) =
                            (word(slot), word(slot + U256::ONE))
                        else {
                            return Err(ContractError::StorageReadFailed {
                                context: format!(
                                    "eth_getProof response missing tick {tick} storage slots"
                                ),
                                source: None,
                            }
                            .into());
                        };
                        funding.insert(
                            tick,
                            Ok(TickFunding {
                                cuml_funding_opp_x96: I256::from_raw(opp),
                                cuml_funding_div_sqrt_p_opp_x96: I256::from_raw(div_sqrt_p_opp),
                            }),
                        );
                    }
                    return Ok(funding);
                }
                Err(e) if method_unsupported(&e) => {
                    tracing::debug!(
                        error = %e,
                        "eth_getProof unsupported by endpoint; \
                         falling back to eth_getStorageAt"
                    );
                    self.get_proof_unsupported.store(true, Ordering::Relaxed);
                }
                Err(e) => {
                    return Err(ContractError::StorageReadFailed {
                        context: "tick funding eth_getProof".into(),
                        source: Some(Arc::new(e)),
                    }
                    .into());
                }
            }
        }

        // Fallback: all reads are pinned to the same block and independent,
        // so every tick's slot pair runs concurrently (bounded so a large
        // ladder cannot flood the endpoint).
        let tick_reads = ticks.iter().map(|&tick| {
            let slot = perp_tick_slot(tick);
            let read = |slot: U256| {
                self.provider
                    .get_storage_at(perp_addr, slot)
                    .block_id(block_id)
                    .into_future()
            };
            async move {
                let funding = tokio::try_join!(read(slot), read(slot + U256::ONE)).map(
                    |(opp, div_sqrt_p_opp)| TickFunding {
                        cuml_funding_opp_x96: I256::from_raw(opp),
                        cuml_funding_div_sqrt_p_opp_x96: I256::from_raw(div_sqrt_p_opp),
                    },
                );
                (tick, funding)
            }
        });
        let reads: Vec<_> = stream::iter(tick_reads)
            .buffered(TICK_READ_CONCURRENCY)
            .collect()
            .await;
        let mut funding = BTreeMap::new();
        for (tick, read) in reads {
            match read {
                Ok(read) => {
                    funding.insert(tick, Ok(read));
                }
                Err(e) => {
                    tracing::warn!(tick, error = %e, "maker equity: tick funding read failed");
                    funding.insert(tick, Err(Arc::new(e)));
                }
            }
        }
        Ok(funding)
    }

    // ── Cache helpers ───────────────────────────────────────────────

    /// Get fees from cache or fetch from the `IFees` module at `fees_addr`.
    async fn get_or_fetch_fees(&self, fees_addr: Address) -> Result<Fees> {
        let now_ts = now_secs();
        let key: [u8; 20] = fees_addr.into();

        let cached = {
            let cache = self.state_cache.lock().unwrap();
            cache.get_fees(&key, now_ts).cloned()
        };

        match cached {
            Some(cached) => Ok(Fees::from(cached)),
            None => {
                let fees = self.fetch_fees(fees_addr).await?;
                let mut cache = self.state_cache.lock().unwrap();
                cache.put_fees(key, CachedFees::from(fees), now_ts);
                Ok(fees)
            }
        }
    }

    /// Get bounds from cache or fetch from the `IMarginRatios` module at `ratios_addr`.
    async fn get_or_fetch_bounds(&self, ratios_addr: Address) -> Result<Bounds> {
        let now_ts = now_secs();
        let key: [u8; 20] = ratios_addr.into();

        let cached = {
            let cache = self.state_cache.lock().unwrap();
            cache.get_bounds(&key, now_ts).cloned()
        };

        match cached {
            Some(cached) => Ok(Bounds::from(cached)),
            None => {
                let bounds = self.fetch_bounds(ratios_addr).await?;
                let mut cache = self.state_cache.lock().unwrap();
                cache.put_bounds(key, CachedBounds::from(bounds), now_ts);
                Ok(bounds)
            }
        }
    }

    /// Fetch fees from the `IFees` module contract.
    async fn fetch_fees(&self, fees_addr: Address) -> Result<Fees> {
        if fees_addr == Address::ZERO {
            return Err(ContractError::ModuleNotRegistered {
                module: "IFees".into(),
            }
            .into());
        }

        let fees_contract = IFees::new(fees_addr, &self.provider);

        let fee_result = fees_contract.fees().call().await?;
        let c_fee = u24_to_u32(fee_result.cFee);
        let ins_fee = u24_to_u32(fee_result.insFee);
        let lp_fee = u24_to_u32(fee_result.lpFee);

        let liq_fee = u24_to_u32(fees_contract.liqFee().call().await?);

        let scale = SCALE_F64;
        Ok(Fees {
            creator_fee: c_fee as f64 / scale,
            insurance_fee: ins_fee as f64 / scale,
            lp_fee: lp_fee as f64 / scale,
            liquidation_fee: liq_fee as f64 / scale,
        })
    }

    /// Fetch taker margin-ratio bounds from the `IMarginRatios` module contract.
    async fn fetch_bounds(&self, ratios_addr: Address) -> Result<Bounds> {
        if ratios_addr == Address::ZERO {
            return Err(ContractError::ModuleNotRegistered {
                module: "IMarginRatios".into(),
            }
            .into());
        }

        let ratios_contract = IMarginRatios::new(ratios_addr, &self.provider);
        let taker = ratios_contract.takerMarginRatios().call().await?;

        let scale = SCALE_F64;
        Ok(Bounds {
            min_margin: scale_from_6dec(crate::constants::MIN_OPENING_MARGIN as i128),
            // The initial margin ratio is the minimum margin → maximum leverage.
            min_taker_leverage: 1.0,
            max_taker_leverage: margin_ratio_to_leverage(u24_to_u32(taker.init))?,
            liquidation_taker_ratio: u24_to_u32(taker.liq) as f64 / scale,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{Capacity, MakerFunding};
    use alloy::primitives::I256;

    fn ok_row(return_data: Vec<u8>) -> IMulticall3::Result {
        IMulticall3::Result {
            success: true,
            returnData: return_data.into(),
        }
    }

    fn position_row() -> IMulticall3::Result {
        ok_row(Perp::positionsCall::abi_encode_returns(&Position {
            delta: I256::ZERO,
            margin: 1_000_000,
            liqMarginRatio: alloy::primitives::Uint::<24, 1>::from(50_000u32),
            backstopMarginRatio: alloy::primitives::Uint::<24, 1>::from(25_000u32),
            lastCumlFundingX96: I256::ZERO,
        }))
    }

    fn maker_row(liquidity: u128) -> IMulticall3::Result {
        ok_row(Perp::makerDetailsCall::abi_encode_returns(&Maker {
            tickLower: alloy::primitives::Signed::<24, 1>::try_from(-60).unwrap(),
            tickUpper: alloy::primitives::Signed::<24, 1>::try_from(60).unwrap(),
            liquidity,
            lastLongUtilEarningsX96: U256::ZERO,
            lastShortUtilEarningsX96: U256::ZERO,
            capacity: Capacity { long: 0, short: 0 },
            lastCumlFunding: MakerFunding {
                belowX96: I256::ZERO,
                withinX96: I256::ZERO,
                divSqrtPriceWithinX96: I256::ZERO,
            },
        }))
    }

    /// One bad position row must degrade alone: the other ids in the batch
    /// keep their pending/skipped classification and their input order.
    #[test]
    fn split_maker_rows_degrades_one_bad_row_alone() {
        let pos_ids = [U256::from(11u8), U256::from(22u8), U256::from(33u8)];
        let reverted = IMulticall3::Result {
            success: false,
            returnData: Vec::new().into(),
        };
        let rows = vec![
            position_row(),
            maker_row(1_000), // pos 11: open maker → pending
            position_row(),
            reverted, // pos 22: failed row → degrades alone
            position_row(),
            maker_row(0), // pos 33: zero liquidity → skipped
        ];

        let (pending, failed) = split_maker_rows(&pos_ids, &rows);

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].input_index, 0);
        assert_eq!(pending[0].pos_id, U256::from(11u8));
        assert_eq!((pending[0].tick_lower, pending[0].tick_upper), (-60, 60));

        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, 1);
        assert_eq!(failed[0].1, U256::from(22u8));
    }

    fn pending_maker(input_index: usize, pos_id: u8, lower: i32, upper: i32) -> PendingMaker {
        PendingMaker {
            input_index,
            pos_id: U256::from(pos_id),
            position: Position {
                delta: I256::ZERO,
                margin: 0,
                liqMarginRatio: alloy::primitives::Uint::<24, 1>::ZERO,
                backstopMarginRatio: alloy::primitives::Uint::<24, 1>::ZERO,
                lastCumlFundingX96: I256::ZERO,
            },
            details: Maker {
                tickLower: alloy::primitives::Signed::<24, 1>::try_from(lower).unwrap(),
                tickUpper: alloy::primitives::Signed::<24, 1>::try_from(upper).unwrap(),
                liquidity: 1,
                lastLongUtilEarningsX96: U256::ZERO,
                lastShortUtilEarningsX96: U256::ZERO,
                capacity: Capacity { long: 0, short: 0 },
                lastCumlFunding: MakerFunding {
                    belowX96: I256::ZERO,
                    withinX96: I256::ZERO,
                    divSqrtPriceWithinX96: I256::ZERO,
                },
            },
            tick_lower: lower,
            tick_upper: upper,
        }
    }

    /// The extsload layout must read each DISTINCT band tick once — a maker
    /// ladder shares its inner boundaries — and index every word back to the
    /// right slot. Locks the `1 + n_ticks + n_positions` word arithmetic.
    #[test]
    fn fee_growth_layout_dedups_ticks_and_indexes_words() {
        let pool_id = B256::repeat_byte(0xAB);
        let perp = Address::repeat_byte(0xCD);
        // A ladder: three positions, four distinct ticks (60 and 120 shared).
        let pending = [
            pending_maker(0, 1, -60, 60),
            pending_maker(1, 2, 60, 120),
            pending_maker(2, 3, 120, 180),
        ];
        let layout = FeeGrowthLayout::new(pool_id, perp, &pending);

        // 1 global + 4 distinct ticks + 3 positions.
        assert_eq!(layout.word_count(), 8);
        assert_eq!(
            layout.slots[0],
            B256::from(v4_fee_growth_global1_slot(pool_id))
        );
        for tick in [-60, 60, 120, 180] {
            assert_eq!(
                layout.slots[layout.tick_word[&tick]],
                B256::from(v4_tick_fee_growth_outside1_slot(pool_id, tick)),
                "outside slot for tick {tick}"
            );
        }
        for (i, maker) in pending.iter().enumerate() {
            assert_eq!(
                layout.slots[layout.first_inside_word + i],
                B256::from(v4_position_fee_growth_inside1_slot(
                    pool_id,
                    perp,
                    maker.tick_lower,
                    maker.tick_upper,
                    B256::from(maker.pos_id),
                )),
                "inside slot for position {i}"
            );
        }

        // Word extraction follows the same indices.
        let words: Vec<B256> = (0u8..8).map(B256::repeat_byte).collect();
        assert_eq!(layout.global(&words), U256::from_be_bytes(words[0].0));
        assert_eq!(
            layout.outside(&words, 60),
            U256::from_be_bytes(words[layout.tick_word[&60]].0)
        );
        assert_eq!(
            layout.inside_last(&words, 2),
            U256::from_be_bytes(words[layout.first_inside_word + 2].0)
        );
    }

    /// A failed tick read must fail exactly the positions referencing that
    /// tick: both neighbours of a shared failed boundary degrade, while a
    /// position whose band avoids it computes normally.
    #[test]
    fn failed_tick_read_degrades_only_referencing_positions() {
        let shared_error = std::sync::Arc::new(alloy::transports::TransportErrorKind::custom_str(
            "replica dropped the read",
        ));
        let funding: BTreeMap<i32, TickFundingRead> = BTreeMap::from([
            (-60, Ok(TickFunding::default())),
            (60, Err(std::sync::Arc::clone(&shared_error))),
            (120, Ok(TickFunding::default())),
            (180, Ok(TickFunding::default())),
        ]);

        // Bands (-60,60) and (60,120) reference the failed tick; (120,180)
        // does not.
        assert!(tick_funding_for(&funding, -60).is_ok());
        assert!(tick_funding_for(&funding, 120).is_ok());
        assert!(tick_funding_for(&funding, 180).is_ok());
        let err = tick_funding_for(&funding, 60).unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::StorageReadFailed {
                    source: Some(_),
                    ..
                })
            ),
            "{err}"
        );
    }
}
