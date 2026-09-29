//! Read operations: market data, positions, balances, and multicall batches.
//!
//! The client is bound to a single `Perp` market, the one it was built with. There is
//! no `PerpManager` and no `perp_id` — the market is identified by which `Perp`
//! contract the client points at. Positions are keyed by `posId` (ERC721 token
//! id) within that `Perp`.
//!
//! Pre-trade quoting (the old `quote_*` / `quoteClosePosition` family) is not
//! available: the frozen `Perp` exposes no on-chain quote/preview views. That
//! surface will return in a later stage (off-chain math or `eth_call`
//! simulation).

use std::collections::BTreeMap;

use alloy::eips::BlockId;
use alloy::primitives::{Address, B256, U256};
use alloy::providers::{MulticallError, Provider};

use crate::constants::{MAX_SWAP_SQRT_PRICE_X96, MAX_TICK, MIN_SWAP_SQRT_PRICE_X96, MIN_TICK};
use crate::contracts::{
    IBeacon, IFees, IMarginRatios, IPoolManagerState, IPriceImpact, Perp, Position,
};
use crate::convert::{margin_ratio_to_leverage, price_x96_to_f64, scale_from_6dec};
use crate::errors::{ContractError, PerpCityError, Result, ValidationError};
use crate::hft::state_cache::{CachedBounds, CachedFees};
use crate::math::BlockContext;
use crate::math::capacity::MarketCapacity;
use crate::math::ema::{PricePair, calculate_emas};
use crate::math::pricing::{FairPrice, fair_price_x96};
use crate::math::swap::{TakerMarketSnapshot, TickLiquidity};
use crate::storage::{perp_emas_slot, v4_tick_bitmap_slot, v4_tick_slot};
use crate::types::{
    Bounds, Fees, MarginRatioTriple, MarginRatios, OpenInterest, PerpData, PerpSnapshot,
};

use super::market::MarketReader;
use super::{PerpClient, SCALE_F64, i24_to_i32, now_secs, u24_to_u32};

/// Funding/utilization rates are scaled by 1e18 per day on-chain.
const WAD_F64: f64 = 1e18;

/// A module address from `modules()`, rejecting the zero address (an
/// unregistered module) with a typed error naming the interface.
pub(super) fn registered_module(addr: Address, module: &str) -> Result<Address> {
    if addr == Address::ZERO {
        return Err(ContractError::ModuleNotRegistered {
            module: module.into(),
        }
        .into());
    }
    Ok(addr)
}

/// Map a failed [`PerpClient::multicall_at`] batch onto the SDK's errors.
///
/// A transport failure keeps the classification a contract call gets
/// ([`PerpCityError::Abi`]): a view that reverts reaches the client as a
/// node error response, and retrying it cannot help.
pub(super) fn multicall_error(e: MulticallError) -> PerpCityError {
    match e {
        MulticallError::TransportError(e) => alloy::contract::Error::TransportError(e).into(),
        MulticallError::DecodeError(e) => ValidationError::DecodeFailed {
            context: format!("failed to decode a multicall result: {e}"),
        }
        .into(),
        e => ContractError::MulticallFailed {
            reason: e.to_string(),
        }
        .into(),
    }
}

/// The contract's `EMA_WINDOW` (seconds) narrowed to the width
/// `calculate_emas` takes.
pub(super) fn ema_window_secs(ema_window: U256) -> Result<u64> {
    u64::try_from(ema_window).map_err(|_| {
        ValidationError::Overflow {
            context: "EMA window".into(),
        }
        .into()
    })
}

/// The Perp views the contract's mark depends on, read at one block.
pub(super) struct MarkViews {
    /// `modules().beacon`.
    pub(super) beacon: Address,
    /// `poolState().ammPrice`.
    pub(super) amm_price_x96: U256,
    /// `emas()`: the stored pair as of `last_touch`.
    pub(super) stored_emas: PricePair,
    /// `rates().lastTouch`.
    pub(super) last_touch: u64,
    /// `EMA_WINDOW()`.
    pub(super) ema_window: U256,
}

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

impl MarketReader {
    // ── Read operations ──────────────────────────────────────────────

    /// Cache key for this market: the `Perp` address left-padded to 32 bytes.
    fn market_key(&self) -> [u8; 32] {
        self.perp.into_word().0
    }

    /// The deployment-fixed values the taker book loader needs, from the
    /// chain reader's per-market cache, or three RPC reads the first time
    /// any reader of this market asks.
    async fn book_immutables(&self) -> Result<BookImmutables> {
        {
            let cached = self.chain.immutables().lock().unwrap();
            if let Some(immutables) = cached.get(&self.perp) {
                return Ok(*immutables);
            }
        }

        let perp = Perp::new(self.perp, self.chain.provider());
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
        let immutables = BookImmutables {
            pool_id,
            tick_spacing,
            ema_window: ema_window_secs(ema_window)?,
        };

        self.chain
            .immutables()
            .lock()
            .unwrap()
            .insert(self.perp, immutables);
        Ok(immutables)
    }

    /// The contract's mark at `block`, as `PerpLogic.accrue` sets it: the
    /// deployed fair price of the pool price and the beacon index, with the
    /// stored EMAs advanced to the block timestamp.
    ///
    /// `index()` mutates on chain; an `eth_call` pinned to the block reads
    /// it without sending. The beacon guard names the missing interface on
    /// a perp with no beacon, where the bare call returns an opaque
    /// ABI-decode error.
    pub(super) async fn contract_mark_x96(
        &self,
        block: &BlockContext,
        views: MarkViews,
    ) -> Result<U256> {
        let beacon = registered_module(views.beacon, "IBeacon")?;
        let index = IBeacon::new(beacon, self.chain.provider())
            .index()
            .block(BlockId::hash(block.hash))
            .call()
            .await?;
        let emas = calculate_emas(
            views.stored_emas,
            PricePair::try_from_x96(views.amm_price_x96, index)?,
            views.last_touch,
            block.timestamp,
            ema_window_secs(views.ema_window)?,
        )?;
        Ok(fair_price_x96(
            views.amm_price_x96,
            index,
            U256::from(emas.amm),
            U256::from(emas.index),
        ))
    }

    /// Read the contract's mark: the deployed fair price
    /// ([`fair_price_x96`]) of the pool price, the beacon index and the
    /// EMAs advanced to the block, pinned to one lagged block (see
    /// [`SNAPSHOT_BLOCK_LAG`](crate::constants::SNAPSHOT_BLOCK_LAG)).
    ///
    /// This is the price every health check, `valPnl` and utilization
    /// accrual uses, and the mark [`Self::get_maker_equities`] prices at.
    /// [`Self::get_mark_price`] returns the pool price instead.
    ///
    /// # Errors
    ///
    /// [`ContractError::ModuleNotRegistered`] when the perp has no beacon;
    /// [`ContractError::BlockUnavailable`] when the pinned header is missing
    /// from the serving replica.
    pub async fn get_fair_price(&self) -> Result<FairPrice> {
        let (block, block_id) = self.chain.lagged_snapshot_block().await?;
        let perp = Perp::new(self.perp, self.chain.provider());
        let (modules, pool_state, stored_emas, rates, ema_window) = self
            .chain
            .multicall_at(block_id)
            .add(perp.modules())
            .add(perp.poolState())
            .add(perp.emas())
            .add(perp.rates())
            .add(perp.EMA_WINDOW())
            .aggregate()
            .await
            .map_err(multicall_error)?;
        let views = MarkViews {
            beacon: modules.beacon,
            amm_price_x96: pool_state.ammPrice,
            stored_emas: PricePair {
                amm: stored_emas.ammPrice,
                index: stored_emas.index,
            },
            last_touch: rates.lastTouch.to::<u64>(),
            ema_window,
        };
        let price_x96 = self.contract_mark_x96(&block, views).await?;
        Ok(FairPrice { block, price_x96 })
    }

    /// Load an exact concentrated-liquidity snapshot at a lagged canonical
    /// block for the deployed Perp contract (`perpcity-contracts@4bbe554f`).
    ///
    /// The loader reads the stored EMA pair from its deployed storage slot,
    /// advances it with the contract's exact arithmetic, and evaluates the
    /// configured price-impact module. Every contract and PoolManager read is
    /// pinned to the returned block hash, resolved via the same
    /// [`SNAPSHOT_BLOCK_LAG`](crate::constants::SNAPSHOT_BLOCK_LAG) policy as the maker-equity reads.
    pub async fn load_taker_market_snapshot(&self) -> Result<TakerMarketSnapshot> {
        let immutables = self.book_immutables().await?;
        let (block, block_id) = self.chain.lagged_snapshot_block().await?;
        let perp = Perp::new(self.perp, self.chain.provider());
        let stored_emas = self
            .chain
            .provider()
            .get_storage_at(self.perp, perp_emas_slot())
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
        let index = IBeacon::new(modules.beacon, self.chain.provider())
            .index()
            .block(block_id)
            .call()
            .await?;
        let stored = PricePair {
            amm: (stored_emas & U256::from(u128::MAX)).to::<u128>(),
            index: (stored_emas >> 128usize).to::<u128>(),
        };
        let spot = PricePair::try_from_x96(state.ammPrice, index)?;
        let emas = calculate_emas(
            stored,
            spot,
            rates.lastTouch.to::<u64>(),
            block.timestamp,
            immutables.ema_window,
        )?;
        let bounds = IPriceImpact::new(modules.priceImpact, self.chain.provider())
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
            block,
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
        } = self.book_immutables().await?;

        let min_word = MIN_TICK.div_euclid(spacing).div_euclid(256);
        let max_word = MAX_TICK.div_euclid(spacing).div_euclid(256);
        let bitmap_slots: Vec<B256> = (min_word..=max_word)
            .map(|word| B256::from(v4_tick_bitmap_slot(pool_id, word)))
            .collect();
        let manager =
            IPoolManagerState::new(self.chain.deployments().pool_manager, self.chain.provider());
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
        let perp = Perp::new(self.perp, self.chain.provider());

        let modules = perp.modules().call().await?;
        let pool_key = perp.poolKey().call().await?;
        let pool_state = perp.poolState().call().await?;
        let mark = price_x96_to_f64(pool_state.ammPrice)?;

        let fees = self.get_or_fetch_fees(modules.fees).await?;
        let bounds = self.get_or_fetch_bounds(modules.marginRatios).await?;

        Ok(PerpData {
            perp: self.perp,
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
        let perp = Perp::new(self.perp, self.chain.provider());
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
        let perp = Perp::new(self.perp, self.chain.provider());
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
        let perp = Perp::new(self.perp, self.chain.provider());
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

    /// Get the pool (AMM spot) price via `poolState`. Uses the fast cache
    /// layer (2s TTL).
    ///
    /// Despite the name, this is not the price the contract marks at: every
    /// health check and `valPnl` prices at `fairPrice(ammPrice, index,
    /// emas…)` — see [`crate::math::pricing`]. Use this for the pool's spot
    /// state; use the maker/taker snapshots for contract-consistent marks.
    pub async fn get_mark_price(&self) -> Result<f64> {
        let now_ts = now_secs();
        let key = self.market_key();

        // Check cache
        {
            let cache = self.chain.state_cache().lock().unwrap();
            if let Some(price) = cache.get_mark_price(&key, now_ts) {
                tracing::trace!(price, "mark price cache hit");
                return Ok(price);
            }
        }

        // Fetch from chain
        let perp = Perp::new(self.perp, self.chain.provider());
        let pool_state = perp.poolState().call().await?;
        let price = price_x96_to_f64(pool_state.ammPrice)?;

        tracing::debug!(price, "mark price fetched");

        // Update cache
        {
            let mut cache = self.chain.state_cache().lock().unwrap();
            cache.put_mark_price(key, price, now_ts);
        }

        Ok(price)
    }

    /// Read the market's `IMarginRatios` module: the maker and taker
    /// init / liquidation / backstop thresholds, as fractions.
    ///
    /// `modules()` and the two ratio getters are pinned to one lagged
    /// block, so a governance module swap or ratio update cannot straddle
    /// the result. Not cached. Verified live 2026-09-07 on HORMUZ-TRAFFIC's
    /// module `0x8afca53c52b1f02d76aefb811c6b08f4bd3e4cf9` (Arbitrum One):
    /// `makerMarginRatios()` = (1000000, 900000, 800000), i.e.
    /// 1.0 / 0.9 / 0.8; `takerMarginRatios()` = (100000, 50000, 20000),
    /// i.e. 0.1 / 0.05 / 0.02.
    ///
    /// # Errors
    ///
    /// [`ContractError::ModuleNotRegistered`] when `modules().marginRatios`
    /// is the zero address; [`ContractError::BlockUnavailable`] when the
    /// pinned header is missing from the serving replica.
    pub async fn get_margin_ratios(&self) -> Result<MarginRatios> {
        let (_, block_id) = self.chain.lagged_snapshot_block().await?;
        let perp = Perp::new(self.perp, self.chain.provider());
        let modules = perp.modules().block(block_id).call().await?;
        let ratios = IMarginRatios::new(
            registered_module(modules.marginRatios, "IMarginRatios")?,
            self.chain.provider(),
        );
        let maker_call = ratios.makerMarginRatios().block(block_id);
        let taker_call = ratios.takerMarginRatios().block(block_id);
        let (maker, taker) = tokio::try_join!(maker_call.call(), taker_call.call())?;
        Ok(MarginRatios {
            maker: MarginRatioTriple::from_e6(
                u24_to_u32(maker.init),
                u24_to_u32(maker.liq),
                u24_to_u32(maker.backstop),
            ),
            taker: MarginRatioTriple::from_e6(
                u24_to_u32(taker.init),
                u24_to_u32(taker.liq),
                u24_to_u32(taker.backstop),
            ),
        })
    }

    /// Get taker open interest for the market, in perp tokens.
    ///
    /// Reads the latest block, not the lagged snapshot block. For open
    /// interest in atoms at a known block, next to the capacity it draws
    /// on, use [`Self::get_capacity`].
    pub async fn get_open_interest(&self) -> Result<OpenInterest> {
        let perp = Perp::new(self.perp, self.chain.provider());
        let oi = perp.openInterest().call().await?;

        Ok(OpenInterest {
            long_oi: oi.long as f64 / SCALE_F64,
            short_oi: oi.short as f64 / SCALE_F64,
        })
    }

    /// Read the market's taker capacity and open interest, pinned to one
    /// lagged block (see [`SNAPSHOT_BLOCK_LAG`](crate::constants::SNAPSHOT_BLOCK_LAG)).
    ///
    /// [`MarketCapacity`] derives each side's headroom (open interest a
    /// taker can still add) and utilization as the contract computes it.
    ///
    /// # Errors
    ///
    /// [`ContractError::BlockUnavailable`] when the pinned header is missing
    /// from the serving replica.
    pub async fn get_capacity(&self) -> Result<MarketCapacity> {
        let (block, block_id) = self.chain.lagged_snapshot_block().await?;
        let perp = Perp::new(self.perp, self.chain.provider());
        let (capacity, oi) = self
            .chain
            .multicall_at(block_id)
            .add(perp.capacity())
            .add(perp.openInterest())
            .aggregate()
            .await
            .map_err(multicall_error)?;
        Ok(MarketCapacity {
            block,
            capacity: capacity.into(),
            long_open_interest_atoms: oi.long,
            short_open_interest_atoms: oi.short,
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
            let cache = self.chain.state_cache().lock().unwrap();
            if let Some(rate) = cache.get_funding_rate(&key, now_ts) {
                tracing::trace!(rate, "funding rate cache hit");
                return Ok(rate);
            }
        }

        let perp = Perp::new(self.perp, self.chain.provider());
        let rates = perp.rates().call().await?;
        let daily_rate = funding_per_day_to_f64(rates.fundingPerDay);

        tracing::debug!(daily_rate, "funding rate fetched");

        // Update cache
        {
            let mut cache = self.chain.state_cache().lock().unwrap();
            cache.put_funding_rate(key, daily_rate, now_ts);
        }

        Ok(daily_rate)
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
    ///
    /// Unlike the pinned reads ([`Self::get_capacity`],
    /// [`Self::get_fair_price`]), this reads the latest block, and the
    /// multicall and the beacon read are separate calls, so a trade between
    /// them can put the index one block after the pool state.
    pub async fn get_perp_snapshot(&self) -> Result<(PerpData, PerpSnapshot)> {
        let perp = Perp::new(self.perp, self.chain.provider());
        let (modules, pool_key, pool_state, rates, oi) = self
            .chain
            .multicall_at(BlockId::latest())
            .add(perp.modules())
            .add(perp.poolKey())
            .add(perp.poolState())
            .add(perp.rates())
            .add(perp.openInterest())
            .aggregate()
            .await
            .map_err(multicall_error)?;

        let mark = price_x96_to_f64(pool_state.ammPrice)?;
        let funding_rate_daily = funding_per_day_to_f64(rates.fundingPerDay);
        let open_interest = OpenInterest {
            long_oi: oi.long as f64 / SCALE_F64,
            short_oi: oi.short as f64 / SCALE_F64,
        };

        // Index price from the beacon (1 CU).
        let index_price = self.chain.get_index_price(modules.beacon).await?;

        // Fees/bounds (from cache or chain).
        let fees = self.get_or_fetch_fees(modules.fees).await?;
        let bounds = self.get_or_fetch_bounds(modules.marginRatios).await?;

        let perp_data = PerpData {
            perp: self.perp,
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

    // ── Cache helpers ───────────────────────────────────────────────

    /// Get fees from cache or fetch from the `IFees` module at `fees_addr`.
    async fn get_or_fetch_fees(&self, fees_addr: Address) -> Result<Fees> {
        let now_ts = now_secs();
        let key: [u8; 20] = fees_addr.into();

        let cached = {
            let cache = self.chain.state_cache().lock().unwrap();
            cache.get_fees(&key, now_ts).cloned()
        };

        match cached {
            Some(cached) => Ok(Fees::from(cached)),
            None => {
                let fees = self.fetch_fees(fees_addr).await?;
                let mut cache = self.chain.state_cache().lock().unwrap();
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
            let cache = self.chain.state_cache().lock().unwrap();
            cache.get_bounds(&key, now_ts).cloned()
        };

        match cached {
            Some(cached) => Ok(Bounds::from(cached)),
            None => {
                let bounds = self.fetch_bounds(ratios_addr).await?;
                let mut cache = self.chain.state_cache().lock().unwrap();
                cache.put_bounds(key, CachedBounds::from(bounds), now_ts);
                Ok(bounds)
            }
        }
    }

    /// Fetch fees from the `IFees` module contract.
    async fn fetch_fees(&self, fees_addr: Address) -> Result<Fees> {
        let fees_contract = IFees::new(
            registered_module(fees_addr, "IFees")?,
            self.chain.provider(),
        );

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
        let ratios_contract = IMarginRatios::new(
            registered_module(ratios_addr, "IMarginRatios")?,
            self.chain.provider(),
        );
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

impl PerpClient {
    // ── Reads, on this client's market and chain readers ─────────────

    /// [`MarketReader::get_fair_price`] on this client's market.
    pub async fn get_fair_price(&self) -> Result<FairPrice> {
        self.market.get_fair_price().await
    }

    /// [`MarketReader::load_taker_market_snapshot`] on this client's market.
    pub async fn load_taker_market_snapshot(&self) -> Result<TakerMarketSnapshot> {
        self.market.load_taker_market_snapshot().await
    }

    /// [`MarketReader::get_perp_config`] on this client's market.
    pub async fn get_perp_config(&self) -> Result<PerpData> {
        self.market.get_perp_config().await
    }

    /// [`MarketReader::get_perp_data`] on this client's market.
    pub async fn get_perp_data(&self) -> Result<(Address, i32, f64)> {
        self.market.get_perp_data().await
    }

    /// [`MarketReader::get_position`] on this client's market.
    pub async fn get_position(&self, pos_id: U256) -> Result<Position> {
        self.market.get_position(pos_id).await
    }

    /// [`MarketReader::get_positions_by_owner`] on this client's market.
    pub async fn get_positions_by_owner(&self, owner: Address) -> Result<Vec<U256>> {
        self.market.get_positions_by_owner(owner).await
    }

    /// [`MarketReader::get_mark_price`] on this client's market.
    pub async fn get_mark_price(&self) -> Result<f64> {
        self.market.get_mark_price().await
    }

    /// [`MarketReader::get_margin_ratios`] on this client's market.
    pub async fn get_margin_ratios(&self) -> Result<MarginRatios> {
        self.market.get_margin_ratios().await
    }

    /// [`MarketReader::get_open_interest`] on this client's market.
    pub async fn get_open_interest(&self) -> Result<OpenInterest> {
        self.market.get_open_interest().await
    }

    /// [`MarketReader::get_capacity`] on this client's market.
    pub async fn get_capacity(&self) -> Result<MarketCapacity> {
        self.market.get_capacity().await
    }

    /// [`MarketReader::get_funding_rate`] on this client's market.
    pub async fn get_funding_rate(&self) -> Result<f64> {
        self.market.get_funding_rate().await
    }

    /// [`MarketReader::get_perp_snapshot`] on this client's market.
    pub async fn get_perp_snapshot(&self) -> Result<(PerpData, PerpSnapshot)> {
        self.market.get_perp_snapshot().await
    }

    /// [`ChainReader::get_index_price`](super::ChainReader::get_index_price)
    /// on this client's chain reader.
    pub async fn get_index_price(&self, beacon: Address) -> Result<f64> {
        self.chain.get_index_price(beacon).await
    }

    /// The signer's USDC balance:
    /// [`ChainReader::balance_of`](super::ChainReader::balance_of) at this
    /// client's address.
    pub async fn get_usdc_balance(&self) -> Result<f64> {
        self.chain.balance_of(self.address).await
    }

    /// [`ChainReader::get_balances`](super::ChainReader::get_balances) on
    /// this client's chain reader.
    pub async fn get_balances(&self, address: Address) -> Result<(f64, U256)> {
        self.chain.get_balances(address).await
    }

    /// [`ChainReader::get_balances_batch`](super::ChainReader::get_balances_batch)
    /// on this client's chain reader.
    pub async fn get_balances_batch(&self, addresses: &[Address]) -> Result<Vec<(f64, U256)>> {
        self.chain.get_balances_batch(addresses).await
    }
}

/// Characterisation of the reads as deployed: each test pins what a read
/// asks the chain, what it makes of the answer, and what it keeps.
#[cfg(test)]
mod tests {
    use alloy::primitives::Uint;

    use super::*;
    use crate::client::mock::{self, BEACON, Rpc, e6, failed_row, ok_row, returns, x96};
    use crate::constants::SNAPSHOT_BLOCK_LAG;
    use crate::contracts::{IERC20, IMulticall3, Modules, Rates};
    use crate::errors::PerpCityError;
    use crate::math::capacity::Capacity;

    /// The market's tick spacing in these tests.
    const SPACING: i32 = 30;

    /// The three `Perp` reads that open `get_perp_config` and
    /// `get_perp_data`: modules, pool key, pool state.
    fn perp_answers(rpc: &Rpc) {
        rpc.call::<Perp::modulesCall>(&mock::modules());
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key(SPACING));
        rpc.call::<Perp::poolStateCall>(&mock::pool_state(x96(3, 1)));
    }

    /// The `IFees` module's two reads, and what they decode to.
    fn fees_answers(rpc: &Rpc) {
        rpc.call::<IFees::feesCall>(&IFees::feesReturn {
            cFee: e6(1_000),
            insFee: e6(2_000),
            lpFee: e6(3_000),
        });
        rpc.call::<IFees::liqFeeCall>(&e6(50_000));
    }

    fn expected_fees() -> Fees {
        Fees {
            creator_fee: 0.001,
            insurance_fee: 0.002,
            lp_fee: 0.003,
            liquidation_fee: 0.05,
        }
    }

    /// The `IMarginRatios` module's taker read, and what it decodes to.
    fn bounds_answers(rpc: &Rpc) {
        rpc.call::<IMarginRatios::takerMarginRatiosCall>(&IMarginRatios::takerMarginRatiosReturn {
            init: e6(100_000),
            liq: e6(50_000),
            backstop: e6(20_000),
        });
    }

    fn expected_bounds() -> Bounds {
        Bounds {
            min_margin: 5.0,
            min_taker_leverage: 1.0,
            max_taker_leverage: 10.0,
            liquidation_taker_ratio: 0.05,
        }
    }

    // ── Fast layer: mark, funding, balance ────────────────────────────

    /// `poolState().ammPrice` is Q96; the read hands back the plain price.
    #[tokio::test]
    async fn mark_price_is_the_pool_price_scaled_from_x96() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::poolStateCall>(&mock::pool_state(x96(3, 1)));

        assert_eq!(client.get_mark_price().await.unwrap(), 1.5);
        assert!(rpc.is_drained(), "one eth_call");
    }

    /// The fast layer answers a second read without an RPC — the queue is
    /// empty, so an RPC would fail — until the caller invalidates it.
    #[tokio::test]
    async fn mark_price_is_served_from_the_fast_layer_until_invalidated() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::poolStateCall>(&mock::pool_state(x96(3, 1)));
        assert_eq!(client.get_mark_price().await.unwrap(), 1.5);

        assert_eq!(client.get_mark_price().await.unwrap(), 1.5, "cache hit");

        client.invalidate_fast_cache();
        rpc.call::<Perp::poolStateCall>(&mock::pool_state(x96(1, 0)));
        assert_eq!(client.get_mark_price().await.unwrap(), 1.0);
        assert!(rpc.is_drained());
    }

    /// `rates().fundingPerDay` is an `int88` scaled by 1e18; the sign is
    /// the contract's (positive: longs pay shorts).
    #[tokio::test]
    async fn funding_rate_scales_the_per_day_rate_from_wad() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::ratesCall>(&mock::rates(-5_000_000_000_000_000));

        assert_eq!(client.get_funding_rate().await.unwrap(), -0.005);
        assert_eq!(
            client.get_funding_rate().await.unwrap(),
            -0.005,
            "cache hit"
        );

        client.invalidate_fast_cache();
        rpc.call::<Perp::ratesCall>(&mock::rates(2_000_000_000_000_000));
        assert_eq!(client.get_funding_rate().await.unwrap(), 0.002);
        assert!(rpc.is_drained());
    }

    /// The signer's USDC balance, scaled from six decimals, and cached in
    /// the fast layer alongside the prices.
    #[tokio::test]
    async fn usdc_balance_is_scaled_from_6dec_and_cached() {
        let (client, rpc) = mock::client();
        rpc.call::<IERC20::balanceOfCall>(&U256::from(1_234_567u32));

        assert_eq!(client.get_usdc_balance().await.unwrap(), 1.234_567);
        assert_eq!(
            client.get_usdc_balance().await.unwrap(),
            1.234_567,
            "cache hit"
        );

        client.invalidate_fast_cache();
        rpc.call::<IERC20::balanceOfCall>(&U256::ZERO);
        assert_eq!(client.get_usdc_balance().await.unwrap(), 0.0);
        assert!(rpc.is_drained());
    }

    /// A balance beyond `i128` is an overflow, not a saturated number.
    #[tokio::test]
    async fn usdc_balance_beyond_i128_is_an_overflow() {
        let (client, rpc) = mock::client();
        rpc.call::<IERC20::balanceOfCall>(&(U256::from(i128::MAX) + U256::from(1u8)));

        let err = client.get_usdc_balance().await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Validation(ValidationError::Overflow { .. })
            ),
            "{err}"
        );
    }

    // ── Failure surface ───────────────────────────────────────────────

    /// A transport failure inside a contract call surfaces as an ABI
    /// error wrapping the transport error — which [`PerpCityError::
    /// is_transient`] does not classify as transient. Recorded as is;
    /// the classification is #115.
    #[tokio::test]
    async fn a_transport_failure_in_a_contract_call_is_an_abi_error() {
        let (client, rpc) = mock::client();
        rpc.fails("connection reset");

        let err = client.get_mark_price().await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Abi(alloy::contract::Error::TransportError(_))
            ),
            "{err}"
        );
        assert!(!err.is_transient(), "not classified transient today");
    }

    // ── Uncached single reads ─────────────────────────────────────────

    /// The beacon's `index()` is Q96, read by `eth_call` without sending.
    #[tokio::test]
    async fn index_price_is_the_beacon_index_scaled_from_x96() {
        let (client, rpc) = mock::client();
        rpc.call::<IBeacon::indexCall>(&x96(5, 2));

        assert_eq!(client.get_index_price(BEACON).await.unwrap(), 1.25);
        assert!(rpc.is_drained(), "one eth_call, nothing cached");
    }

    /// A zero index is a beacon with nothing to say, not a price of zero.
    #[tokio::test]
    async fn a_zero_index_is_an_invalid_price() {
        let (client, rpc) = mock::client();
        rpc.call::<IBeacon::indexCall>(&U256::ZERO);

        let err = client.get_index_price(BEACON).await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Validation(ValidationError::InvalidPrice { .. })
            ),
            "{err}"
        );
    }

    /// `positions(id)` comes back as the raw contract struct, untouched.
    #[tokio::test]
    async fn a_position_is_the_raw_contract_struct() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::positionsCall>(&mock::position(1_000_000));

        let position = client.get_position(U256::from(7u8)).await.unwrap();
        assert_eq!(position.margin, 1_000_000);
        assert!(position.delta.is_zero());
        assert!(rpc.is_drained());
    }

    /// The contract answers a burned or never-minted id with an all-zero
    /// struct rather than a revert; the read names it.
    #[tokio::test]
    async fn an_all_zero_position_is_not_found() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::positionsCall>(&mock::position(0));

        let Err(err) = client.get_position(U256::from(7u8)).await else {
            panic!("an all-zero struct must not decode as a position");
        };
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::PositionNotFound { pos_id })
                    if pos_id == U256::from(7u8)
            ),
            "{err}"
        );
    }

    /// Open interest is stored in perp atoms (six decimals).
    #[tokio::test]
    async fn open_interest_scales_atoms_to_perp_tokens() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::openInterestCall>(&mock::open_interest(1_500_000, 250_000));

        let oi = client.get_open_interest().await.unwrap();
        assert_eq!((oi.long_oi, oi.short_oi), (1.5, 0.25));
        assert!(rpc.is_drained());
    }

    // ── Positions by owner: an `ownerOf` walk ─────────────────────────

    /// Every id below `nextPosId` is asked; a revert means a burned token
    /// and is skipped, another owner's token is skipped, and the walk
    /// costs one RPC per id plus the counter.
    #[tokio::test]
    async fn positions_by_owner_walks_every_minted_id_and_skips_burned_ones() {
        let owner = Address::repeat_byte(0xaa);
        let (client, rpc) = mock::client();
        rpc.call::<Perp::nextPosIdCall>(&U256::from(4u8));
        rpc.call::<Perp::ownerOfCall>(&owner);
        rpc.call::<Perp::ownerOfCall>(&Address::repeat_byte(0xbb));
        rpc.reverts(&[0xde, 0xad, 0xbe, 0xef]);

        let owned = client.get_positions_by_owner(owner).await.unwrap();
        assert_eq!(owned, vec![U256::from(1u8)]);
        assert!(rpc.is_drained(), "nextPosId plus one ownerOf per id");
    }

    /// A node error with no revert data is a network condition, not a
    /// burned token: the walk stops and says so instead of dropping ids.
    #[tokio::test]
    async fn positions_by_owner_propagates_a_transport_failure() {
        let owner = Address::repeat_byte(0xaa);
        let (client, rpc) = mock::client();
        rpc.call::<Perp::nextPosIdCall>(&U256::from(3u8));
        rpc.call::<Perp::ownerOfCall>(&owner);
        rpc.fails("connection reset");

        let err = client.get_positions_by_owner(owner).await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Abi(alloy::contract::Error::TransportError(_))
            ),
            "{err}"
        );
    }

    /// Ids start at 1, so a `nextPosId` of 0 or 1 means nothing was ever
    /// minted, and the counter is the only RPC.
    #[tokio::test]
    async fn positions_by_owner_of_an_empty_market_is_one_rpc() {
        let owner = Address::repeat_byte(0xaa);
        for next in [0u8, 1] {
            let (client, rpc) = mock::client();
            rpc.call::<Perp::nextPosIdCall>(&U256::from(next));

            assert!(
                client
                    .get_positions_by_owner(owner)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(rpc.is_drained());
        }
    }

    /// The walk is bounded by `u64`; a counter beyond it is an overflow,
    /// not a walk that never ends.
    #[tokio::test]
    async fn positions_by_owner_rejects_a_counter_beyond_u64() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::nextPosIdCall>(&(U256::from(u64::MAX) + U256::from(1u8)));

        let err = client
            .get_positions_by_owner(Address::repeat_byte(0xaa))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Validation(ValidationError::Overflow { .. })
            ),
            "{err}"
        );
    }

    // ── Slow layer: fees and bounds, keyed by module address ──────────

    /// The config read asks the `Perp` for its modules, then asks the
    /// modules it named: six RPCs, decoded into fractions.
    #[tokio::test]
    async fn perp_config_reads_the_perp_then_the_modules_it_names() {
        let (client, rpc) = mock::client();
        perp_answers(&rpc);
        fees_answers(&rpc);
        bounds_answers(&rpc);

        let config = client.get_perp_config().await.unwrap();
        assert_eq!(
            config,
            PerpData {
                perp: mock::PERP,
                tick_spacing: SPACING,
                mark: 1.5,
                beacon: BEACON,
                bounds: expected_bounds(),
                fees: expected_fees(),
            }
        );
        assert!(
            rpc.is_drained(),
            "modules, poolKey, poolState, fees, liqFee, takerMarginRatios"
        );
    }

    /// Fees and bounds live in the slow layer: a second config read asks
    /// only the `Perp`, `invalidate_fast_cache` leaves them in place, and
    /// `invalidate_all_cache` is what evicts them.
    #[tokio::test]
    async fn fees_and_bounds_survive_a_fast_invalidation_but_not_a_full_one() {
        let (client, rpc) = mock::client();
        perp_answers(&rpc);
        fees_answers(&rpc);
        bounds_answers(&rpc);
        let first = client.get_perp_config().await.unwrap();

        perp_answers(&rpc);
        assert_eq!(client.get_perp_config().await.unwrap(), first);
        assert!(rpc.is_drained(), "the modules were not asked again");

        client.invalidate_fast_cache();
        perp_answers(&rpc);
        assert_eq!(client.get_perp_config().await.unwrap(), first);
        assert!(rpc.is_drained(), "the slow layer is untouched");

        client.invalidate_all_cache();
        perp_answers(&rpc);
        fees_answers(&rpc);
        bounds_answers(&rpc);
        assert_eq!(client.get_perp_config().await.unwrap(), first);
        assert!(rpc.is_drained(), "evicted: the modules are asked again");
    }

    /// A zero module address is rejected by name before it is called, so
    /// no RPC goes to the zero address.
    #[tokio::test]
    async fn an_unregistered_module_is_named_before_it_is_asked() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::modulesCall>(&Modules {
            fees: Address::ZERO,
            ..mock::modules()
        });
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key(SPACING));
        rpc.call::<Perp::poolStateCall>(&mock::pool_state(x96(3, 1)));

        let err = client.get_perp_config().await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::ModuleNotRegistered { ref module })
                    if module == "IFees"
            ),
            "{err}"
        );
        assert!(
            rpc.is_drained(),
            "the perp was read; the fees module was not"
        );
    }

    /// The lighter read stops at the `Perp`: beacon, spacing, mark.
    #[tokio::test]
    async fn perp_data_stops_at_the_perp() {
        let (client, rpc) = mock::client();
        perp_answers(&rpc);

        assert_eq!(
            client.get_perp_data().await.unwrap(),
            (BEACON, SPACING, 1.5)
        );
        assert!(rpc.is_drained());
    }

    // ── Pinned reads: head minus the lag ──────────────────────────────

    /// Both triples come back as fractions. The two reads share one ABI
    /// shape, so distinct values are what pin their order.
    #[tokio::test]
    async fn margin_ratios_decode_both_triples_as_fractions() {
        let (client, rpc) = mock::client();
        rpc.quantity(100);
        rpc.block(100 - SNAPSHOT_BLOCK_LAG, 1_700_000_000);
        rpc.call::<Perp::modulesCall>(&mock::modules());
        rpc.call::<IMarginRatios::makerMarginRatiosCall>(&IMarginRatios::makerMarginRatiosReturn {
            init: e6(1_000_000),
            liq: e6(900_000),
            backstop: e6(800_000),
        });
        rpc.call::<IMarginRatios::takerMarginRatiosCall>(&IMarginRatios::takerMarginRatiosReturn {
            init: e6(100_000),
            liq: e6(50_000),
            backstop: e6(20_000),
        });

        let ratios = client.get_margin_ratios().await.unwrap();
        assert_eq!(
            (
                ratios.maker.init,
                ratios.maker.liquidation,
                ratios.maker.backstop
            ),
            (1.0, 0.9, 0.8)
        );
        assert_eq!(
            (
                ratios.taker.init,
                ratios.taker.liquidation,
                ratios.taker.backstop
            ),
            (0.1, 0.05, 0.02)
        );
        assert!(
            rpc.is_drained(),
            "blockNumber, block, modules, maker, taker"
        );
    }

    /// The pinned block is the head less the lag, and a replica that has
    /// no header for it is a failed read that names the block — and is
    /// transient, since the replica will catch up.
    #[tokio::test]
    async fn a_missing_lagged_header_is_block_unavailable_at_head_minus_lag() {
        let (client, rpc) = mock::client();
        rpc.quantity(100);
        rpc.no_block();

        let err = client.get_margin_ratios().await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::BlockUnavailable { number })
                    if number == 100 - SNAPSHOT_BLOCK_LAG
            ),
            "{err}"
        );
        assert!(err.is_transient());
        assert!(
            rpc.is_drained(),
            "nothing is read at a block that is missing"
        );
    }

    /// A market with no ratios module fails after the modules read, not
    /// with an opaque decode error from the zero address.
    #[tokio::test]
    async fn margin_ratios_without_a_module_fail_after_the_modules_read() {
        let (client, rpc) = mock::client();
        rpc.quantity(100);
        rpc.block(100 - SNAPSHOT_BLOCK_LAG, 1_700_000_000);
        rpc.call::<Perp::modulesCall>(&Modules {
            marginRatios: Address::ZERO,
            ..mock::modules()
        });

        let err = client.get_margin_ratios().await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::ModuleNotRegistered { ref module })
                    if module == "IMarginRatios"
            ),
            "{err}"
        );
        assert!(rpc.is_drained());
    }

    // ── Multicall reads ───────────────────────────────────────────────

    /// N addresses cost one RPC: N `balanceOf` rows then N `getEthBalance`
    /// rows, paired back up in input order.
    #[tokio::test]
    async fn balances_batch_bundles_usdc_then_eth_into_one_multicall() {
        let holders = [Address::repeat_byte(0xa1), Address::repeat_byte(0xa2)];
        let (client, rpc) = mock::client();
        rpc.aggregate3(vec![
            ok_row(returns::<IERC20::balanceOfCall>(&U256::from(1_000_000u32))),
            ok_row(returns::<IERC20::balanceOfCall>(&U256::from(2_500_000u32))),
            ok_row(returns::<IMulticall3::getEthBalanceCall>(&U256::from(5u8))),
            ok_row(returns::<IMulticall3::getEthBalanceCall>(&U256::ZERO)),
        ]);

        let balances = client.get_balances_batch(&holders).await.unwrap();
        assert_eq!(balances, vec![(1.0, U256::from(5u8)), (2.5, U256::ZERO)]);
        assert!(rpc.is_drained(), "one eth_call for four sub-calls");
    }

    /// No addresses, no RPC.
    #[tokio::test]
    async fn balances_batch_of_nobody_makes_no_rpc() {
        let (client, rpc) = mock::client();

        assert!(client.get_balances_batch(&[]).await.unwrap().is_empty());
        assert!(rpc.is_drained());
    }

    /// A multicall that answers with the wrong number of rows, or a row
    /// that reverted, is a multicall failure rather than a partial answer.
    #[tokio::test]
    async fn a_short_or_reverted_balance_row_is_a_multicall_failure() {
        let holders = [Address::repeat_byte(0xa1), Address::repeat_byte(0xa2)];
        let usdc = || ok_row(returns::<IERC20::balanceOfCall>(&U256::ZERO));
        let eth = || ok_row(returns::<IMulticall3::getEthBalanceCall>(&U256::ZERO));

        let (client, rpc) = mock::client();
        rpc.aggregate3(vec![usdc(), usdc(), eth()]);
        let err = client.get_balances_batch(&holders).await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::MulticallFailed { .. })
            ),
            "short: {err}"
        );

        rpc.aggregate3(vec![usdc(), failed_row(), eth(), eth()]);
        let err = client.get_balances_batch(&holders).await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::MulticallFailed { .. })
            ),
            "reverted row: {err}"
        );
        assert!(rpc.is_drained());
    }

    /// Capacity and open interest are read in one batch at the lagged
    /// block, and the result carries that block.
    #[tokio::test]
    async fn capacity_carries_the_lagged_block_it_was_read_at() {
        let (client, rpc) = mock::client();
        rpc.quantity(100);
        let hash = rpc.block(100 - SNAPSHOT_BLOCK_LAG, 1_700_000_000);
        rpc.aggregate(
            100 - SNAPSHOT_BLOCK_LAG,
            [
                returns::<Perp::capacityCall>(&mock::capacity(10, 20)),
                returns::<Perp::openInterestCall>(&mock::open_interest(3, 4)),
            ],
        );

        assert_eq!(
            client.get_capacity().await.unwrap(),
            MarketCapacity {
                block: BlockContext {
                    number: 100 - SNAPSHOT_BLOCK_LAG,
                    hash,
                    timestamp: 1_700_000_000,
                },
                capacity: Capacity {
                    long_atoms: 10,
                    short_atoms: 20,
                },
                long_open_interest_atoms: 3,
                short_open_interest_atoms: 4,
            }
        );
        assert!(rpc.is_drained(), "blockNumber, block, one multicall");
    }

    /// The snapshot is one multicall for the `Perp`'s five views, one
    /// beacon read, and the slow layer — which a second snapshot skips.
    #[tokio::test]
    async fn perp_snapshot_is_one_multicall_plus_the_beacon_and_the_slow_layer() {
        let perp_views = || {
            [
                returns::<Perp::modulesCall>(&mock::modules()),
                returns::<Perp::poolKeyCall>(&mock::pool_key(SPACING)),
                returns::<Perp::poolStateCall>(&mock::pool_state(x96(3, 1))),
                returns::<Perp::ratesCall>(&mock::rates(-5_000_000_000_000_000)),
                returns::<Perp::openInterestCall>(&mock::open_interest(1_500_000, 250_000)),
            ]
        };
        let (client, rpc) = mock::client();
        rpc.aggregate(100, perp_views());
        rpc.call::<IBeacon::indexCall>(&x96(5, 2));
        fees_answers(&rpc);
        bounds_answers(&rpc);

        let (data, snapshot) = client.get_perp_snapshot().await.unwrap();
        assert_eq!(
            data,
            PerpData {
                perp: mock::PERP,
                tick_spacing: SPACING,
                mark: 1.5,
                beacon: BEACON,
                bounds: expected_bounds(),
                fees: expected_fees(),
            }
        );
        assert_eq!(
            snapshot,
            PerpSnapshot {
                mark_price: 1.5,
                index_price: 1.25,
                funding_rate_daily: -0.005,
                open_interest: OpenInterest {
                    long_oi: 1.5,
                    short_oi: 0.25,
                },
            }
        );
        assert!(rpc.is_drained(), "multicall, index, fees, liqFee, ratios");

        rpc.aggregate(101, perp_views());
        rpc.call::<IBeacon::indexCall>(&x96(5, 2));
        assert_eq!(client.get_perp_snapshot().await.unwrap(), (data, snapshot));
        assert!(rpc.is_drained(), "fees and bounds came from the slow layer");
    }

    /// With pool, index and EMAs all equal and no time since the last
    /// touch, the fair price is that price: the plumbing is pinned without
    /// re-deriving the EMA math, which has its own tests.
    #[tokio::test]
    async fn fair_price_reads_the_mark_inputs_at_the_lagged_block() {
        const TOUCHED_AT: u64 = 1_700_000_000;
        let one = x96(1, 0);
        let (client, rpc) = mock::client();
        rpc.quantity(100);
        let hash = rpc.block(100 - SNAPSHOT_BLOCK_LAG, TOUCHED_AT);
        rpc.aggregate(
            100 - SNAPSHOT_BLOCK_LAG,
            [
                returns::<Perp::modulesCall>(&mock::modules()),
                returns::<Perp::poolStateCall>(&mock::pool_state(one)),
                returns::<Perp::emasCall>(&mock::emas(one.to::<u128>(), one.to::<u128>())),
                returns::<Perp::ratesCall>(&Rates {
                    lastTouch: Uint::from(TOUCHED_AT),
                    ..mock::rates(0)
                }),
                returns::<Perp::EMA_WINDOWCall>(&U256::from(3_600u32)),
            ],
        );
        rpc.call::<IBeacon::indexCall>(&one);

        assert_eq!(
            client.get_fair_price().await.unwrap(),
            FairPrice {
                block: BlockContext {
                    number: 100 - SNAPSHOT_BLOCK_LAG,
                    hash,
                    timestamp: TOUCHED_AT,
                },
                price_x96: one,
            }
        );
        assert!(rpc.is_drained(), "blockNumber, block, multicall, index");
    }

    /// A perp with no beacon fails by name after the batch, before the
    /// index read that would otherwise decode nothing from the zero
    /// address.
    #[tokio::test]
    async fn fair_price_without_a_beacon_names_the_missing_interface() {
        let one = x96(1, 0);
        let (client, rpc) = mock::client();
        rpc.quantity(100);
        rpc.block(100 - SNAPSHOT_BLOCK_LAG, 1_700_000_000);
        rpc.aggregate(
            100 - SNAPSHOT_BLOCK_LAG,
            [
                returns::<Perp::modulesCall>(&Modules {
                    beacon: Address::ZERO,
                    ..mock::modules()
                }),
                returns::<Perp::poolStateCall>(&mock::pool_state(one)),
                returns::<Perp::emasCall>(&mock::emas(0, 0)),
                returns::<Perp::ratesCall>(&mock::rates(0)),
                returns::<Perp::EMA_WINDOWCall>(&U256::from(3_600u32)),
            ],
        );

        let err = client.get_fair_price().await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::ModuleNotRegistered { ref module })
                    if module == "IBeacon"
            ),
            "{err}"
        );
        assert!(rpc.is_drained());
    }

    // ── The taker snapshot: immutables, pinned views, and the book ────

    /// The pool's id, as `POOL_ID()` reports it.
    const POOL_ID: B256 = B256::repeat_byte(0x99);
    /// When the market was last touched, and the block time in these
    /// tests, so the stored EMAs need no advancing.
    const TOUCHED_AT: u64 = 1_700_000_000;
    /// Bitmap words the book walk covers at this spacing:
    /// `MIN_TICK.div_euclid(30) = -4606`, `.div_euclid(256) = -18`, up to
    /// `4606.div_euclid(256) = 17` — 36 words, word `w` at offset `w + 18`.
    const BITMAP_WORDS: usize = 36;

    /// The three immutables the first snapshot reads, in `try_join!` order.
    fn immutables_answers(rpc: &Rpc) {
        rpc.call::<Perp::POOL_IDCall>(&POOL_ID);
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key(SPACING));
        rpc.call::<Perp::EMA_WINDOWCall>(&U256::from(3_600u32));
    }

    /// Everything after the immutables: the lagged block, the stored
    /// EMAs, the three pinned `Perp` views, the beacon, the impact bounds,
    /// then the bitmap and — if any tick is set — the tick words. Pool,
    /// index and EMAs are all 1.0; returns the pinned block's hash.
    fn snapshot_answers(
        rpc: &Rpc,
        liquidity: u128,
        bitmap: Vec<B256>,
        tick_words: Option<Vec<B256>>,
    ) -> B256 {
        let one = x96(1, 0);
        rpc.quantity(100);
        let hash = rpc.block(100 - SNAPSHOT_BLOCK_LAG, TOUCHED_AT);
        rpc.storage((one << 128) | one);
        rpc.call::<Perp::poolStateCall>(&Perp::poolStateReturn {
            sqrtPrice: Uint::from(1u8) << 96,
            liquidity,
            ..mock::pool_state(one)
        });
        rpc.call::<Perp::modulesCall>(&mock::modules());
        rpc.call::<Perp::ratesCall>(&Rates {
            lastTouch: Uint::from(TOUCHED_AT),
            ..mock::rates(0)
        });
        rpc.call::<IBeacon::indexCall>(&one);
        rpc.call::<IPriceImpact::sqrtPriceBoundsCall>(&IPriceImpact::sqrtPriceBoundsReturn {
            sqrtMin: one >> 1,
            sqrtMax: one << 1,
        });
        rpc.call::<IPoolManagerState::extsload_1Call>(&bitmap);
        if let Some(words) = tick_words {
            rpc.call::<IPoolManagerState::extsload_1Call>(&words);
        }
        hash
    }

    /// A bitmap with these `(offset, bit)`s set.
    fn bitmap(set: &[(usize, usize)]) -> Vec<B256> {
        let mut words = vec![U256::ZERO; BITMAP_WORDS];
        for &(offset, bit) in set {
            words[offset] |= U256::from(1u8) << bit;
        }
        words.into_iter().map(B256::from).collect()
    }

    /// A tick's storage word: `liquidityNet` in the high half,
    /// `liquidityGross` in the low.
    fn tick_word(gross: u128, net: i128) -> B256 {
        B256::from((U256::from(net as u128) << 128) | U256::from(gross))
    }

    fn expected_snapshot(hash: B256, liquidity: u128) -> TakerMarketSnapshot {
        let one = x96(1, 0);
        TakerMarketSnapshot {
            block: BlockContext {
                number: 100 - SNAPSHOT_BLOCK_LAG,
                hash,
                timestamp: TOUCHED_AT,
            },
            sqrt_price_x96: one,
            tick: 0,
            liquidity,
            ticks: BTreeMap::new(),
            protocol_sqrt_min_x96: MIN_SWAP_SQRT_PRICE_X96,
            protocol_sqrt_max_x96: MAX_SWAP_SQRT_PRICE_X96,
            impact_sqrt_min_x96: one >> 1,
            impact_sqrt_max_x96: one << 1,
        }
    }

    /// An empty bitmap means no tick words are asked for, and the empty
    /// book reconciles with a pool holding no liquidity.
    #[tokio::test]
    async fn taker_snapshot_of_an_empty_book_skips_the_tick_words() {
        let (client, rpc) = mock::client();
        immutables_answers(&rpc);
        let hash = snapshot_answers(&rpc, 0, bitmap(&[]), None);

        assert_eq!(
            client.load_taker_market_snapshot().await.unwrap(),
            expected_snapshot(hash, 0)
        );
        assert!(rpc.is_drained(), "twelve answers, none left over");
    }

    /// Set bits become ticks at `compressed * spacing`, their words are
    /// read in bitmap order, and the net liquidity of every tick at or
    /// below the pool's must add up to what the pool reports.
    #[tokio::test]
    async fn taker_snapshot_rebuilds_the_book_from_the_bitmap() {
        const L: u128 = 1_000;
        let (client, rpc) = mock::client();
        immutables_answers(&rpc);
        // Tick -60 is compressed -2: word -1 (offset 17), bit 254.
        // Tick 60 is compressed 2: word 0 (offset 18), bit 2.
        let hash = snapshot_answers(
            &rpc,
            L,
            bitmap(&[(17, 254), (18, 2)]),
            Some(vec![tick_word(L, L as i128), tick_word(L, -(L as i128))]),
        );

        let snapshot = client.load_taker_market_snapshot().await.unwrap();
        assert_eq!(
            snapshot,
            TakerMarketSnapshot {
                ticks: BTreeMap::from([
                    (
                        -60,
                        TickLiquidity {
                            gross: L,
                            net: L as i128
                        }
                    ),
                    (
                        60,
                        TickLiquidity {
                            gross: L,
                            net: -(L as i128)
                        }
                    ),
                ]),
                ..expected_snapshot(hash, L)
            }
        );
        assert!(rpc.is_drained(), "thirteen answers");
    }

    /// A book whose ticks do not add up to the pool's active liquidity is
    /// a wrong read, not a snapshot.
    #[tokio::test]
    async fn a_book_that_does_not_reconcile_with_the_pool_is_rejected() {
        const L: u128 = 1_000;
        let (client, rpc) = mock::client();
        immutables_answers(&rpc);
        snapshot_answers(
            &rpc,
            L + 1,
            bitmap(&[(17, 254), (18, 2)]),
            Some(vec![tick_word(L, L as i128), tick_word(L, -(L as i128))]),
        );

        let err = client.load_taker_market_snapshot().await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::MulticallFailed { ref reason })
                    if reason.contains("liquidity mismatch")
            ),
            "{err}"
        );
    }

    /// The immutables are read once per market: a second snapshot starts
    /// at the lagged block.
    #[tokio::test]
    async fn book_immutables_are_read_once_per_market() {
        let (client, rpc) = mock::client();
        immutables_answers(&rpc);
        snapshot_answers(&rpc, 0, bitmap(&[]), None);
        client.load_taker_market_snapshot().await.unwrap();

        let hash = snapshot_answers(&rpc, 0, bitmap(&[]), None);
        assert_eq!(
            client.load_taker_market_snapshot().await.unwrap(),
            expected_snapshot(hash, 0)
        );
        assert!(rpc.is_drained(), "nine answers: no immutables");
    }

    /// The immutables belong to the market, not the reader: a second
    /// reader of the same market over one chain reader inherits them, and
    /// a reader of another market reads its own.
    #[tokio::test]
    async fn book_immutables_are_shared_per_market_across_readers() {
        let (chain, rpc) = mock::chain();
        let (first, second) = (chain.market(mock::PERP), chain.market(mock::PERP));
        immutables_answers(&rpc);
        snapshot_answers(&rpc, 0, bitmap(&[]), None);
        first.load_taker_market_snapshot().await.unwrap();

        snapshot_answers(&rpc, 0, bitmap(&[]), None);
        second.load_taker_market_snapshot().await.unwrap();
        assert!(rpc.is_drained(), "the second reader skipped the immutables");

        let other = chain.market(Address::repeat_byte(0x12));
        immutables_answers(&rpc);
        snapshot_answers(&rpc, 0, bitmap(&[]), None);
        other.load_taker_market_snapshot().await.unwrap();
        assert!(rpc.is_drained(), "another market reads its own");
    }

    /// A non-positive spacing is rejected when the immutables are first
    /// read, and the rejection is not cached: the next snapshot asks again.
    #[tokio::test]
    async fn a_non_positive_tick_spacing_is_rejected_and_asked_again() {
        let (client, rpc) = mock::client();
        for spacing in [0, -30] {
            rpc.call::<Perp::POOL_IDCall>(&POOL_ID);
            rpc.call::<Perp::poolKeyCall>(&mock::pool_key(spacing));
            rpc.call::<Perp::EMA_WINDOWCall>(&U256::from(3_600u32));

            let err = client.load_taker_market_snapshot().await.unwrap_err();
            assert!(
                matches!(
                    err,
                    PerpCityError::Validation(ValidationError::InvalidConfig { .. })
                ),
                "{err}"
            );
            assert!(rpc.is_drained(), "three answers each time");
        }
    }
}
