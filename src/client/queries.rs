//! Reads of the market as it is now.
//!
//! Every read here is independently current — served from the state cache
//! within its TTL, or from the chain head at the moment of the call — and
//! makes no promise about agreeing with any other read on the block it came
//! from. Reads that must agree, or that name a block, are on
//! [`StateAt`](super::StateAt): the handle is the block.
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

use alloy::primitives::{Address, B256, U256};
use alloy::providers::MulticallError;
use serde::{Deserialize, Serialize};

use crate::contracts::{self, IFees, IMarginRatios, Perp, Position};
use crate::convert::{price_x96_to_f64, scale_from_6dec};
use crate::errors::{ContractError, PerpCityError, Result, ValidationError};
use crate::hft::state_cache::{CachedBounds, CachedFees};
use crate::history::TapeAddresses;
use crate::math::BlockContext;
use crate::math::pricing::{Emas, Mark};
use crate::units::{PerSide, PerpAtoms, Price, Ratio};

use super::market::MarketReader;
use super::state::{ema_window_secs, pinned_read_error};
use super::{PerpClient, i24_to_i32, now_secs, u24_to_u32};

/// Funding/utilization rates are scaled by 1e18 per day on-chain.
const WAD_F64: f64 = 1e18;

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
    /// The pool's (AMM spot) price at the read — not the contract's mark,
    /// which is the fair price ([`crate::math::pricing`]).
    pub pool_price: Price,
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
    /// Margin ratio at which taker liquidation occurs.
    pub liquidation_taker_ratio: Ratio,
}

/// A market's fee rates, each a share of the thing it is charged on.
///
/// The contract holds all four as `uint24` at 1e6, so `1_000` is 0.1%, and
/// `fraction()` on any of them is the number a person reads. They are
/// [`Ratio`] rather than `f64` because a liquidation's fee *rate* and the
/// USDC a liquidation *settles* are both called the liquidation fee, and as
/// two floats they substituted for each other.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Fees {
    /// Share paid to the perp creator.
    pub creator_fee: Ratio,
    /// Share that goes to the insurance fund.
    pub insurance_fee: Ratio,
    /// Share earned by liquidity providers.
    pub lp_fee: Ratio,
    /// Share charged on a liquidation, applied to the position's value.
    pub liquidation_fee: Ratio,
}

/// Taker open interest, per side, as the contract accumulates it: the sum
/// of `|perp_delta|` over the open positions on that side, in perp atoms.
/// [`PerpAtoms::value_at`] prices it.
pub type OpenInterest = PerSide<PerpAtoms>;

impl From<contracts::OpenInterest> for OpenInterest {
    fn from(oi: contracts::OpenInterest) -> Self {
        Self::new(PerpAtoms::new(oi.long), PerpAtoms::new(oi.short))
    }
}

/// The market's live state at the lagged snapshot block, in the chain's
/// units.
///
/// Pure market state — no configuration. Returned alongside
/// [`MarketConfig`] from [`MarketReader::get_snapshot`].
/// What a live cache seeds from before it follows the feed: the prices,
/// the contract's mark, and the stored EMAs it needs to keep marking
/// between touches, each the same type the events then carry.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MarketSnapshot {
    /// The block every field was read at: the lagged snapshot block, with
    /// its hash, so further reads can pin to it.
    pub block: BlockContext,
    /// The pool's (AMM spot) price — not a TWAP, and not the contract's
    /// mark, which is the fair price ([`crate::math::pricing`]).
    pub pool_price: Price,
    /// The beacon's index.
    pub index_price: Price,
    /// The contract's mark at the block: the fair price of the pool price,
    /// the index and the EMAs advanced to the block's timestamp, exact.
    /// What every health check, `valPnl` and liquidation prices at, so the
    /// basis the contract sees is this against the index, not the pool
    /// price against it.
    pub mark: Price,
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

/// A price from a chain word, refusing zero: no market has a zero price, so
/// one is a read of an uninitialised pool or beacon, and it must not reach
/// a cache that would mark from it.
fn read_price(x96: U256, what: &str) -> Result<Price> {
    let price = Price::from_x96(x96);
    if price.is_zero() {
        return Err(ValidationError::InvalidPrice {
            reason: format!("{what} read as zero"),
        }
        .into());
    }
    Ok(price)
}

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

/// Which contract build a market runs. The two live builds share every
/// view and trade selector; they differ in how a liquidation is called and
/// in what a close event carries, and the SDK picks by era where it must.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Era {
    /// Build `58b42b7`: whole-position 2-arg liquidations, tailed close
    /// events, a hookless pool.
    Legacy,
    /// `v0.2.2-upgradeable` (tag `198559a`): an ERC-1967 proxy, 3-arg
    /// liquidations by amount, untailed close events plus `*Liquidated`,
    /// a pool guarded by the `PerpGuardHook`.
    Upgradeable,
}

impl Era {
    /// The era a pool key names: only `v0.2.2` pools carry a hook.
    pub(crate) fn from_hooks(hooks: Address) -> Self {
        if hooks == Address::ZERO {
            Self::Legacy
        } else {
            Self::Upgradeable
        }
    }
}

/// Perp/pool values fixed at deployment, cached after the first read that
/// needs one. All are Solidity `immutable`s (or built from them), so no
/// block pinning is needed and they can never go stale.
#[derive(Debug, Clone, Copy)]
pub(super) struct MarketImmutables {
    /// Uniswap V4 `PoolId` of the market's pool.
    pub(super) pool_id: B256,
    /// Pool tick spacing (validated positive at load).
    pub(super) tick_spacing: i32,
    /// The contract build, from the pool key's hook.
    pub(super) era: Era,
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

    /// The deployment-fixed values the pool snapshot and the liquidation
    /// calls need, from the chain reader's per-market cache, or two RPC
    /// reads the first time any reader of this market asks. Not a block's
    /// to give, so it lives here rather than on the handle.
    pub(super) async fn immutables(&self) -> Result<MarketImmutables> {
        {
            let cached = self.chain.immutables_cache().lock().unwrap();
            if let Some(immutables) = cached.get(&self.perp) {
                return Ok(*immutables);
            }
        }

        let perp = Perp::new(self.perp, self.chain.provider());
        let pool_id_call = perp.POOL_ID();
        let pool_key_call = perp.poolKey();
        let (pool_id, pool_key) = tokio::try_join!(pool_id_call.call(), pool_key_call.call())?;
        let tick_spacing = i24_to_i32(pool_key.tickSpacing);
        if tick_spacing <= 0 {
            return Err(ValidationError::InvalidConfig {
                reason: format!("invalid tick spacing {tick_spacing}"),
            }
            .into());
        }
        let immutables = MarketImmutables {
            pool_id,
            tick_spacing,
            era: Era::from_hooks(pool_key.hooks),
        };

        self.chain
            .immutables_cache()
            .lock()
            .unwrap()
            .insert(self.perp, immutables);
        Ok(immutables)
    }

    /// The three addresses this market's tape is read from
    /// ([`History::market_tape`](crate::history::History::market_tape)):
    /// the perp, the beacon it is configured with now, and the chain's
    /// PoolManager with this market's pool id.
    ///
    /// The beacon is governance's to change, so this is the beacon at the
    /// time of the call; a tape read from the market's genesis carries
    /// every `SetBeacon` and a fold learns the earlier ones from it.
    ///
    /// # Errors
    ///
    /// [`ContractError::ModuleNotRegistered`] when the perp has no beacon;
    /// the transport error from either read.
    pub async fn tape_addresses(&self) -> Result<TapeAddresses> {
        let perp = Perp::new(self.perp, self.chain.provider());
        let (modules, immutables) = tokio::try_join!(
            async { perp.modules().call().await.map_err(PerpCityError::from) },
            self.immutables(),
        )?;
        Ok(TapeAddresses {
            perp: self.perp,
            beacon: registered_module(modules.beacon, "IBeacon")?,
            pool_manager: self.chain.deployments().pool_manager,
            pool_id: immutables.pool_id,
        })
    }

    /// [`StateAt::mark`](super::StateAt::mark) at the lagged snapshot
    /// block: what the contract marks from, and through
    /// [`Mark::fair_price`] the price it marks at.
    ///
    /// That is the price every health check, `valPnl` and utilization
    /// accrual uses, and the mark [`Self::get_maker_equities`] prices at.
    /// [`Self::get_pool_price`] is the pool's spot price instead.
    ///
    /// # Errors
    ///
    /// [`ContractError::ModuleNotRegistered`] when the perp has no beacon;
    /// [`ContractError::BlockUnavailable`] when the pinned header is missing
    /// from the serving replica.
    pub async fn get_mark(&self) -> Result<Mark> {
        self.state().await?.mark().await
    }

    /// The market's configuration: its pool's tick spacing, the EMA window,
    /// the beacon it takes its index from, and the bounds and fees its
    /// modules set.
    ///
    /// Uses the [`crate::hft::state_cache::StateCache`] for fees and bounds (60s TTL).
    pub async fn get_config(&self) -> Result<MarketConfig> {
        let perp = Perp::new(self.perp, self.chain.provider());

        let modules = perp.modules().call().await?;
        let pool_key = perp.poolKey().call().await?;
        let pool_state = perp.poolState().call().await?;
        let ema_window = ema_window_secs(perp.EMA_WINDOW().call().await?)?;
        let pool_price = read_price(pool_state.ammPrice, "pool price")?;

        let fees = self.get_or_fetch_fees(modules.fees).await?;
        let bounds = self.get_or_fetch_bounds(modules.marginRatios).await?;

        Ok(MarketConfig {
            perp: self.perp,
            tick_spacing: i24_to_i32(pool_key.tickSpacing),
            ema_window,
            pool_price,
            beacon: modules.beacon,
            bounds,
            fees,
        })
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

    /// The pool's spot price, `poolState().ammPrice`, from the fast cache
    /// layer (2s TTL) or the head.
    ///
    /// Not the price the contract marks at: every health check and
    /// `valPnl` prices at the fair price of the pool price, the index and
    /// the EMAs ([`crate::math::pricing`]), which [`Self::get_mark`] reads.
    pub async fn get_pool_price(&self) -> Result<f64> {
        let now_ts = now_secs();
        let key = self.market_key();

        // Check cache
        {
            let cache = self.chain.state_cache().lock().unwrap();
            if let Some(price) = cache.get_pool_price(&key, now_ts) {
                tracing::trace!(price, "pool price cache hit");
                return Ok(price);
            }
        }

        // Fetch from chain
        let perp = Perp::new(self.perp, self.chain.provider());
        let pool_state = perp.poolState().call().await?;
        let price = price_x96_to_f64(pool_state.ammPrice)?;

        tracing::debug!(price, "pool price fetched");

        // Update cache
        {
            let mut cache = self.chain.state_cache().lock().unwrap();
            cache.put_pool_price(key, price, now_ts);
        }

        Ok(price)
    }

    /// Get taker open interest for the market.
    ///
    /// Reads the latest block. For open interest at a known block, next to
    /// the capacity it draws on, use
    /// [`StateAt::capacity`](super::StateAt::capacity).
    pub async fn get_open_interest(&self) -> Result<OpenInterest> {
        let perp = Perp::new(self.perp, self.chain.provider());
        Ok(perp.openInterest().call().await?.into())
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

    /// The market's configuration and its state, in one multicall plus the
    /// beacon index read and the stored EMAs' storage word.
    ///
    /// Resolves the lagged snapshot block (see
    /// [`SNAPSHOT_BLOCK_LAG`](crate::constants::SNAPSHOT_BLOCK_LAG)) from the
    /// node's header, batches the `Perp`'s views at it, then reads `index()`
    /// on the beacon the batch named and the EMA slot together at the same
    /// block: every field of the snapshot is from that one block, which it
    /// carries. The EMAs come from storage because `v0.2.2` markets have no
    /// `emas()` view and both builds keep the pair at the same slot.
    ///
    /// The block comes from the header, not from Multicall3's
    /// `blockAndAggregate`: on Arbitrum `block.number` inside the EVM is the
    /// L1 block number and `blockhash` of it is zero, so it names no block
    /// on the chain being read.
    ///
    /// Both come back because one batch answers both; a caller that wants
    /// only the configuration takes [`Self::get_config`].
    ///
    /// # Errors
    ///
    /// [`ContractError::ModuleNotRegistered`] when the perp has no beacon;
    /// [`ContractError::BlockUnavailable`] (transient) when a read lands on
    /// a replica that does not have the snapshot block.
    pub async fn get_snapshot(&self) -> Result<(MarketConfig, MarketSnapshot)> {
        let perp = Perp::new(self.perp, self.chain.provider());
        let (block, id) = self.chain.lagged_snapshot_block().await?;
        let (modules, pool_key, pool_state, rates, oi, ema_window) = self
            .chain
            .multicall_at(id)
            .add(perp.modules())
            .add(perp.poolKey())
            .add(perp.poolState())
            .add(perp.rates())
            .add(perp.openInterest())
            .add(perp.EMA_WINDOW())
            .aggregate()
            .await
            .map_err(|e| pinned_read_error(multicall_error(e), block.number))?;

        let pool_price = read_price(pool_state.ammPrice, "pool price")?;
        let funding_rate_daily = funding_per_day_to_f64(rates.fundingPerDay);
        let open_interest = OpenInterest::from(oi);

        let beacon = registered_module(modules.beacon, "IBeacon")?;
        let (index_x96, stored_emas) = tokio::try_join!(
            self.chain.index_x96_at(beacon, id),
            self.chain.stored_emas_at(self.perp, id),
        )
        .map_err(|e| pinned_read_error(e, block.number))?;
        let index_price = read_price(index_x96, "index")?;

        // The mark exactly as the contract would set it at this block; the
        // stored pair goes out as it is so a cache can advance it itself.
        let last_touch = rates.lastTouch.to::<u64>();
        let ema_window = ema_window_secs(ema_window)?;
        let mark = Mark::advanced(
            block,
            pool_price,
            index_price,
            stored_emas,
            last_touch,
            ema_window,
        )?
        .fair_price();
        let emas = Emas::stored(stored_emas, last_touch);

        // Fees/bounds (from cache or chain).
        let fees = self.get_or_fetch_fees(modules.fees).await?;
        let bounds = self.get_or_fetch_bounds(modules.marginRatios).await?;

        let config = MarketConfig {
            perp: self.perp,
            tick_spacing: i24_to_i32(pool_key.tickSpacing),
            ema_window,
            pool_price,
            beacon: modules.beacon,
            bounds,
            fees,
        };

        let snapshot = MarketSnapshot {
            block,
            pool_price,
            index_price,
            mark,
            emas,
            funding_rate_daily,
            open_interest,
        };

        tracing::debug!("market snapshot fetched via multicall");
        Ok((config, snapshot))
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
        let liquidation_fee = fees_contract.liqFee().call().await?;

        Ok(Fees {
            creator_fee: Ratio::from_e6(u24_to_u32(fee_result.cFee))?,
            insurance_fee: Ratio::from_e6(u24_to_u32(fee_result.insFee))?,
            lp_fee: Ratio::from_e6(u24_to_u32(fee_result.lpFee))?,
            liquidation_fee: Ratio::from_e6(u24_to_u32(liquidation_fee))?,
        })
    }

    /// Fetch taker margin-ratio bounds from the `IMarginRatios` module contract.
    async fn fetch_bounds(&self, ratios_addr: Address) -> Result<Bounds> {
        let ratios_contract = IMarginRatios::new(
            registered_module(ratios_addr, "IMarginRatios")?,
            self.chain.provider(),
        );
        let taker = ratios_contract.takerMarginRatios().call().await?;

        Ok(Bounds {
            min_margin: scale_from_6dec(crate::constants::MIN_OPENING_MARGIN as i128),
            // The initial margin ratio is the minimum margin → maximum leverage.
            min_taker_leverage: 1.0,
            max_taker_leverage: Ratio::from_e6(u24_to_u32(taker.init))?.leverage()?,
            liquidation_taker_ratio: Ratio::from_e6(u24_to_u32(taker.liq))?,
        })
    }
}

impl PerpClient {
    /// The signer's USDC balance:
    /// [`ChainReader::balance_of`](super::ChainReader::balance_of) at this
    /// client's address. Every other read is on [`Self::market`] or
    /// [`Self::chain`]; this one stays here because only the client knows
    /// whose balance "mine" is.
    pub async fn get_usdc_balance(&self) -> Result<f64> {
        self.chain().balance_of(self.address).await
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
    use crate::contracts::{IBeacon, IERC20, IMulticall3, Modules, Rates};
    use crate::errors::PerpCityError;
    use crate::math::BlockContext;
    use crate::math::pricing::PricePair;
    use crate::units::Side;

    /// The market's tick spacing in these tests.
    const SPACING: i32 = 30;

    /// The three `Perp` reads that open `get_config`: modules, pool key,
    /// pool state.
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

    /// A ratio from its millionths, as the modules store one.
    fn ratio(value: u32) -> Ratio {
        Ratio::from_e6(value).unwrap()
    }

    fn expected_fees() -> Fees {
        Fees {
            creator_fee: ratio(1_000),
            insurance_fee: ratio(2_000),
            lp_fee: ratio(3_000),
            liquidation_fee: ratio(50_000),
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
            liquidation_taker_ratio: ratio(50_000),
        }
    }

    // ── Narrowing a helper to the reads it makes ──────────────────────

    /// What a downstream read helper looks like once it says it only
    /// reads: bounded on the market reader, not the client.
    async fn pool_price_of(market: impl AsRef<MarketReader>) -> Result<f64> {
        market.as_ref().get_pool_price().await
    }

    /// A helper narrowed to `impl AsRef<MarketReader>` still takes the
    /// client a caller already holds, and takes the bare reader too — so
    /// narrowing costs the caller nothing.
    #[tokio::test]
    async fn a_helper_narrowed_to_the_market_reader_accepts_the_client() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::poolStateCall>(&mock::pool_state(x96(3, 1)));

        assert_eq!(pool_price_of(&client).await.unwrap(), 1.5);
        assert_eq!(
            pool_price_of(client.market()).await.unwrap(),
            1.5,
            "cache hit"
        );
        assert!(rpc.is_drained());
    }

    // ── Fast layer: pool price, funding, balance ──────────────────────

    /// `poolState().ammPrice` is Q96; the read hands back the plain price.
    #[tokio::test]
    async fn pool_price_is_pool_state_scaled_from_x96() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::poolStateCall>(&mock::pool_state(x96(3, 1)));

        assert_eq!(client.market().get_pool_price().await.unwrap(), 1.5);
        assert!(rpc.is_drained(), "one eth_call");
    }

    /// The fast layer answers a second read without an RPC — the queue is
    /// empty, so an RPC would fail — until the caller invalidates it.
    #[tokio::test]
    async fn pool_price_is_served_from_the_fast_layer_until_invalidated() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::poolStateCall>(&mock::pool_state(x96(3, 1)));
        assert_eq!(client.market().get_pool_price().await.unwrap(), 1.5);

        assert_eq!(
            client.market().get_pool_price().await.unwrap(),
            1.5,
            "cache hit"
        );

        client.chain().invalidate_fast_cache();
        rpc.call::<Perp::poolStateCall>(&mock::pool_state(x96(1, 0)));
        assert_eq!(client.market().get_pool_price().await.unwrap(), 1.0);
        assert!(rpc.is_drained());
    }

    /// `rates().fundingPerDay` is an `int88` scaled by 1e18; the sign is
    /// the contract's (positive: longs pay shorts).
    #[tokio::test]
    async fn funding_rate_scales_the_per_day_rate_from_wad() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::ratesCall>(&mock::rates(-5_000_000_000_000_000));

        assert_eq!(client.market().get_funding_rate().await.unwrap(), -0.005);
        assert_eq!(
            client.market().get_funding_rate().await.unwrap(),
            -0.005,
            "cache hit"
        );

        client.chain().invalidate_fast_cache();
        rpc.call::<Perp::ratesCall>(&mock::rates(2_000_000_000_000_000));
        assert_eq!(client.market().get_funding_rate().await.unwrap(), 0.002);
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

        client.chain().invalidate_fast_cache();
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

        let err = client.market().get_pool_price().await.unwrap_err();
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

        assert_eq!(client.chain().get_index_price(BEACON).await.unwrap(), 1.25);
        assert!(rpc.is_drained(), "one eth_call, nothing cached");
    }

    /// A zero index is a beacon with nothing to say, not a price of zero.
    #[tokio::test]
    async fn a_zero_index_is_an_invalid_price() {
        let (client, rpc) = mock::client();
        rpc.call::<IBeacon::indexCall>(&U256::ZERO);

        let err = client.chain().get_index_price(BEACON).await.unwrap_err();
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

        let position = client.market().get_position(U256::from(7u8)).await.unwrap();
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

        let Err(err) = client.market().get_position(U256::from(7u8)).await else {
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
    async fn open_interest_is_the_contracts_pair_of_counts() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::openInterestCall>(&mock::open_interest(1_500_000, 250_000));

        let oi = client.market().get_open_interest().await.unwrap();
        assert_eq!(
            (oi.long, oi.short),
            (PerpAtoms::new(1_500_000), PerpAtoms::new(250_000))
        );
        assert_eq!(*oi.on(Side::Short), oi.short);
        assert_eq!(oi.total(), PerpAtoms::new(1_750_000));
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

        let owned = client.market().get_positions_by_owner(owner).await.unwrap();
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

        let err = client
            .market()
            .get_positions_by_owner(owner)
            .await
            .unwrap_err();
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
                    .market()
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
            .market()
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

    /// The four `Perp` reads that open `get_config`: the three of
    /// [`perp_answers`] and the EMA window.
    fn config_answers(rpc: &Rpc) {
        perp_answers(rpc);
        rpc.call::<Perp::EMA_WINDOWCall>(&U256::from(3_600u32));
    }

    /// A pool whose price reads as zero is uninitialised, not a market at
    /// price zero: the read refuses it rather than seeding a cache with it.
    #[tokio::test]
    async fn a_zero_pool_price_is_an_invalid_price() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::modulesCall>(&mock::modules());
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key(SPACING));
        rpc.call::<Perp::poolStateCall>(&mock::pool_state(U256::ZERO));
        rpc.call::<Perp::EMA_WINDOWCall>(&U256::from(3_600u32));

        let err = client.market().get_config().await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Validation(ValidationError::InvalidPrice { .. })
            ),
            "{err}"
        );
    }

    /// The config read asks the `Perp` for its modules, then asks the
    /// modules it named: seven RPCs, decoded into fractions.
    #[tokio::test]
    async fn perp_config_reads_the_perp_then_the_modules_it_names() {
        let (client, rpc) = mock::client();
        config_answers(&rpc);
        fees_answers(&rpc);
        bounds_answers(&rpc);

        let config = client.market().get_config().await.unwrap();
        assert_eq!(
            config,
            MarketConfig {
                perp: mock::PERP,
                tick_spacing: SPACING,
                ema_window: 3_600,
                pool_price: Price::from_x96(x96(3, 1)),
                beacon: BEACON,
                bounds: expected_bounds(),
                fees: expected_fees(),
            }
        );
        assert!(
            rpc.is_drained(),
            "modules, poolKey, poolState, EMA_WINDOW, fees, liqFee, takerMarginRatios"
        );
    }

    /// Fees and bounds live in the slow layer: a second config read asks
    /// only the `Perp`, `invalidate_fast_cache` leaves them in place, and
    /// `invalidate_all_cache` is what evicts them.
    #[tokio::test]
    async fn fees_and_bounds_survive_a_fast_invalidation_but_not_a_full_one() {
        let (client, rpc) = mock::client();
        config_answers(&rpc);
        fees_answers(&rpc);
        bounds_answers(&rpc);
        let first = client.market().get_config().await.unwrap();

        config_answers(&rpc);
        assert_eq!(client.market().get_config().await.unwrap(), first);
        assert!(rpc.is_drained(), "the modules were not asked again");

        client.chain().invalidate_fast_cache();
        config_answers(&rpc);
        assert_eq!(client.market().get_config().await.unwrap(), first);
        assert!(rpc.is_drained(), "the slow layer is untouched");

        client.chain().invalidate_all_cache();
        config_answers(&rpc);
        fees_answers(&rpc);
        bounds_answers(&rpc);
        assert_eq!(client.market().get_config().await.unwrap(), first);
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
        rpc.call::<Perp::EMA_WINDOWCall>(&U256::from(3_600u32));

        let err = client.market().get_config().await.unwrap_err();
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

        let balances = client.chain().get_balances_batch(&holders).await.unwrap();
        assert_eq!(balances, vec![(1.0, U256::from(5u8)), (2.5, U256::ZERO)]);
        assert!(rpc.is_drained(), "one eth_call for four sub-calls");
    }

    /// No addresses, no RPC.
    #[tokio::test]
    async fn balances_batch_of_nobody_makes_no_rpc() {
        let (client, rpc) = mock::client();

        assert!(
            client
                .chain()
                .get_balances_batch(&[])
                .await
                .unwrap()
                .is_empty()
        );
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
        let err = client
            .chain()
            .get_balances_batch(&holders)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::MulticallFailed { .. })
            ),
            "short: {err}"
        );

        rpc.aggregate3(vec![usdc(), failed_row(), eth(), eth()]);
        let err = client
            .chain()
            .get_balances_batch(&holders)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::MulticallFailed { .. })
            ),
            "reverted row: {err}"
        );
        assert!(rpc.is_drained());
    }

    /// The block time of every snapshot in these tests, and the market's
    /// last touch, so the stored EMAs need no advancing.
    const SNAPSHOT_TIME: u64 = 1_700_000_000;

    /// The six `Perp` views the snapshot batches: pool price 1.5, last
    /// touched at the block time.
    fn snapshot_views() -> [Vec<u8>; 6] {
        [
            returns::<Perp::modulesCall>(&mock::modules()),
            returns::<Perp::poolKeyCall>(&mock::pool_key(SPACING)),
            returns::<Perp::poolStateCall>(&mock::pool_state(x96(3, 1))),
            returns::<Perp::ratesCall>(&Rates {
                lastTouch: Uint::from(SNAPSHOT_TIME),
                ..mock::rates(-5_000_000_000_000_000)
            }),
            returns::<Perp::openInterestCall>(&mock::open_interest(1_500_000, 250_000)),
            returns::<Perp::EMA_WINDOWCall>(&U256::from(3_600u32)),
        ]
    }

    /// The beacon's index and the stored EMAs' word, read together after
    /// the batch: index 1.25, both EMAs 1.0.
    fn index_and_emas_answers(rpc: &Rpc) {
        let one = x96(1, 0).to::<u128>();
        rpc.call::<IBeacon::indexCall>(&x96(5, 2));
        rpc.storage(mock::emas_word(one, one));
    }

    /// Answer the head and the lagged header the snapshot pins to, so the
    /// snapshot's block is `number`; returns the header's hash.
    fn snapshot_block(rpc: &Rpc, number: u64) -> B256 {
        rpc.quantity(number + SNAPSHOT_BLOCK_LAG);
        rpc.block(number, SNAPSHOT_TIME)
    }

    /// The snapshot is the lagged block, one multicall for the `Perp`'s
    /// six views, the beacon and the EMA slot at that block, and the slow
    /// layer — which a second snapshot skips. It carries that block, the
    /// mark priced from the stored EMAs, and the stored pair itself.
    #[tokio::test]
    async fn perp_snapshot_is_one_block_plus_the_slow_layer() {
        let (client, rpc) = mock::client();
        let hash = snapshot_block(&rpc, 100);
        rpc.aggregate(100, snapshot_views());
        index_and_emas_answers(&rpc);
        fees_answers(&rpc);
        bounds_answers(&rpc);

        let (data, snapshot) = client.market().get_snapshot().await.unwrap();
        assert_eq!(
            data,
            MarketConfig {
                perp: mock::PERP,
                tick_spacing: SPACING,
                ema_window: 3_600,
                pool_price: Price::from_x96(x96(3, 1)),
                beacon: BEACON,
                bounds: expected_bounds(),
                fees: expected_fees(),
            }
        );
        let one = Price::from_x96(x96(1, 0));
        assert_eq!(
            snapshot,
            MarketSnapshot {
                block: BlockContext {
                    number: 100,
                    hash,
                    timestamp: SNAPSHOT_TIME,
                },
                pool_price: Price::from_x96(x96(3, 1)),
                index_price: Price::from_x96(x96(5, 2)),
                // fair(1.5, 1.25, 1.0, 1.0) = (1.5 + (1.25 + 1.0 − 1.0)) / 2.
                mark: Price::from_x96(x96(11, 3)),
                emas: Emas {
                    amm_price: one,
                    index: one,
                    last_touch: SNAPSHOT_TIME,
                },
                funding_rate_daily: -0.005,
                open_interest: OpenInterest {
                    long: PerpAtoms::new(1_500_000),
                    short: PerpAtoms::new(250_000),
                },
            }
        );
        assert!(
            rpc.is_drained(),
            "blockNumber, header, multicall, index, emas, fees, liqFee, ratios"
        );

        let hash = snapshot_block(&rpc, 101);
        rpc.aggregate(101, snapshot_views());
        index_and_emas_answers(&rpc);
        assert_eq!(
            client.market().get_snapshot().await.unwrap(),
            (
                data,
                MarketSnapshot {
                    block: BlockContext {
                        number: 101,
                        hash,
                        timestamp: SNAPSHOT_TIME,
                    },
                    ..snapshot
                }
            )
        );
        assert!(rpc.is_drained(), "fees and bounds came from the slow layer");
    }

    /// The block is the node's header, never Multicall3's own
    /// `block.number`: on Arbitrum that is the L1 block number, and a read
    /// pinned to it names no block on the chain.
    #[tokio::test]
    async fn perp_snapshot_block_is_the_header_not_the_evm_block_number() {
        const L1_BLOCK: u64 = 26_090_712;
        let (client, rpc) = mock::client();
        snapshot_block(&rpc, 510_368_453);
        rpc.aggregate(L1_BLOCK, snapshot_views());
        index_and_emas_answers(&rpc);
        fees_answers(&rpc);
        bounds_answers(&rpc);

        let (_, snapshot) = client.market().get_snapshot().await.unwrap();
        assert_eq!(snapshot.block.number, 510_368_453);
        assert!(rpc.is_drained());
    }

    /// Every read is pinned to the snapshot block, so a replica behind the
    /// one that served the header refuses it: a transient failure naming
    /// the block, not an opaque one a retry loop would give up on.
    #[tokio::test]
    async fn perp_snapshot_on_a_lagging_replica_is_block_unavailable() {
        let (client, rpc) = mock::client();
        snapshot_block(&rpc, 100);
        rpc.aggregate(100, snapshot_views());
        rpc.fails("header not found");

        let err = client.market().get_snapshot().await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::BlockUnavailable { number: 100 })
            ),
            "{err}"
        );
        assert!(err.is_transient());
        assert!(rpc.is_drained());
    }

    /// A perp with no beacon fails by name after the batch, before the
    /// index read that would decode nothing from the zero address.
    #[tokio::test]
    async fn perp_snapshot_without_a_beacon_names_the_missing_interface() {
        let (client, rpc) = mock::client();
        let [_, pool_key, pool_state, rates, oi, window] = snapshot_views();
        let modules = returns::<Perp::modulesCall>(&Modules {
            beacon: Address::ZERO,
            ..mock::modules()
        });
        snapshot_block(&rpc, 100);
        rpc.aggregate(100, [modules, pool_key, pool_state, rates, oi, window]);

        let err = client.market().get_snapshot().await.unwrap_err();
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

    /// With pool, index and EMAs all equal and no time since the last
    /// touch, the mark is those inputs and prices at them: the plumbing is
    /// pinned without re-deriving the EMA math, which has its own tests.
    #[tokio::test]
    async fn the_mark_is_read_at_the_lagged_block() {
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
                returns::<Perp::ratesCall>(&Rates {
                    lastTouch: Uint::from(TOUCHED_AT),
                    ..mock::rates(0)
                }),
                returns::<Perp::EMA_WINDOWCall>(&U256::from(3_600u32)),
            ],
        );
        rpc.call::<IBeacon::indexCall>(&one);
        rpc.storage(mock::emas_word(one.to::<u128>(), one.to::<u128>()));

        let mark = client.market().get_mark().await.unwrap();
        assert_eq!(
            mark,
            Mark {
                block: BlockContext {
                    number: 100 - SNAPSHOT_BLOCK_LAG,
                    hash,
                    timestamp: TOUCHED_AT,
                },
                pool_price: Price::from_x96(one),
                index: Price::from_x96(one),
                emas: PricePair {
                    amm: one.to::<u128>(),
                    index: one.to::<u128>(),
                },
            }
        );
        assert_eq!(mark.fair_price(), Price::from_x96(one));
        assert!(
            rpc.is_drained(),
            "blockNumber, block, multicall, index, emas"
        );
    }

    /// A perp with no beacon fails by name after the batch, before the
    /// index read that would otherwise decode nothing from the zero
    /// address.
    #[tokio::test]
    async fn a_mark_without_a_beacon_names_the_missing_interface() {
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
                returns::<Perp::ratesCall>(&mock::rates(0)),
                returns::<Perp::EMA_WINDOWCall>(&U256::from(3_600u32)),
            ],
        );

        let err = client.market().get_mark().await.unwrap_err();
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
}
