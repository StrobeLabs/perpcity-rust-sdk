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

use alloy::eips::BlockId;
use alloy::primitives::{Address, B256, U256};
use alloy::providers::MulticallError;
use serde::{Deserialize, Serialize};

use crate::constants::{
    MAX_PROTOCOL_FEE, MAX_SWAP_SQRT_PRICE_X96, MIN_SWAP_SQRT_PRICE_X96, V1_MAX_PROTOCOL_FEE,
    V1_MAX_SWAP_SQRT_PRICE_X96, V1_MIN_SWAP_SQRT_PRICE_X96,
};
use crate::contracts::{self, IFees, IMarginRatios, Perp, v1};
use crate::convert::{price_x96_to_f64, scale_from_6dec};
use crate::errors::{ContractError, PerpCityError, Result, ValidationError};
use crate::hft::state_cache::{CachedBounds, CachedFees};
use crate::history::TapeAddresses;
use crate::math::BlockContext;
use crate::math::pricing::{Emas, Mark};
use crate::math::swap::TakerFeeRates;
use crate::units::{LUnits, PerSide, PerpAtoms, Price, Ratio, SqrtPrice, UsdcAtoms};

use super::market::MarketReader;
use super::state::{PositionRow, V1Views, ema_window_secs, pinned_read_error};
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
    /// The EMA window, in seconds: the time constant the contract smooths
    /// the pool price and the index with, and what [`Emas::advanced`]
    /// takes. A deployment immutable on the earlier builds (`EMA_WINDOW()`);
    /// on `v1.0.0` governance can change it (`emaWindow()`, `setEmaWindow`),
    /// so it is the window at the read.
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
/// The contract holds every rate as `uint24` at 1e6, so `1_000` is 0.1%,
/// and `fraction()` on any of them is the number a person reads. They are
/// [`Ratio`] rather than `f64` because a liquidation's fee *rate* and the
/// USDC a liquidation *settles* are both called the liquidation fee, and as
/// two floats they substituted for each other.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Fees {
    /// Share paid to the perp creator.
    pub creator_fee: Ratio,
    /// Share that goes to the insurance fund.
    pub insurance_fee: Ratio,
    /// Share earned by liquidity providers: one rate on the earlier
    /// builds, a curve on `v1.0.0`.
    pub lp_fee: LpFee,
    /// Share charged on a liquidation, applied to the position's value.
    pub liquidation_fee: Ratio,
}

/// The LP fee a market's fees module charges a swap.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum LpFee {
    /// The two earlier builds: the module's one `lpFee`, on every swap.
    Fixed(Ratio),
    /// `v1.0.0`: a rate per swap segment, set by the segment's liquidity
    /// and the pool price's distance from its EMA (`Fees._lpFee`), so no
    /// one rate is the market's.
    Curve(LpFeeCurve),
}

/// A `v1.0.0` fees module's LP fee curve (`lpFeeConfig()`) and the cap it
/// keeps the swap fee under. A segment pays `targetFee + (startFee −
/// targetFee) · decay / (liquidity + decay)` as its base, plus the squared
/// relative distance of the pool price from its EMA times the volatility
/// scaler, at most the base times the multiplier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LpFeeCurve {
    /// The base rate at no liquidity.
    pub start_fee: Ratio,
    /// The base rate the curve tends to as liquidity grows.
    pub target_fee: Ratio,
    /// The liquidity at which the base rate is halfway between the two.
    pub liquidity_decay: LUnits,
    /// The weight of the squared price distance, at 1e6.
    pub volatility_scaler: Ratio,
    /// The cap on a segment's rate as a multiple of its base, at 1e6
    /// (`1_500_000` is 1.5 times); never below one.
    pub max_fee_multiplier: Ratio,
    /// `maxSwapFee()`: the most creator, insurance and LP rate together a
    /// segment can pay, the protocol fee excluded. What a trade's fee bound
    /// is sized from.
    pub max_swap_fee: Ratio,
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
    /// The stored EMAs as of the market's last touch. Earlier builds use
    /// [`Emas::mark`]. On v1, [`StateAt::mark`](super::StateAt::mark) asks
    /// the market's pricing module for the advanced pair's mark.
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

/// Which contract build a market runs, read once from its pool key's hook
/// against the chain's [`ChainDeployments`](super::ChainDeployments)
/// ([`ChainDeployments::era_of`](super::ChainDeployments::era_of)) and
/// fixed for the market's life. [`MarketReader::era`] answers it.
///
/// `Legacy` and `Upgradeable` share every view and trade selector and
/// differ in how a liquidation is called, in what a close event carries,
/// and in what their liquidation tests check; the health ports in `math`
/// are the upgradeable build's. `V1` shares no struct shape, trade
/// selector or position event with them ([`crate::contracts::v1`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Era {
    /// Build `58b42b7`: whole-position 2-arg liquidations, tailed close
    /// events, a hookless pool.
    Legacy,
    /// `v0.2.2-upgradeable` (tag `198559a`): an ERC-1967 proxy, 3-arg
    /// liquidations by amount, untailed close events plus `*Liquidated`,
    /// a pool guarded by the chain's `v0.2.2` `PerpGuardHook`.
    Upgradeable,
    /// The audited `v1.0.0` (tag `0df4208`): a pool guarded by the chain's
    /// `v1.0.0` `PerpGuardHook`.
    V1,
}

impl Era {
    /// The sqrt prices a swap on this build stops at: Uniswap's own limits
    /// on the two earlier builds; on `v1.0.0` the maker tick band's edges
    /// (`MIN_SWAP_SQRT_PRICE`, `MAX_SWAP_SQRT_PRICE`), so a swap cannot
    /// rest where no band is active.
    pub fn swap_sqrt_price_limits(self) -> (SqrtPrice, SqrtPrice) {
        let (min, max) = match self {
            Self::Legacy | Self::Upgradeable => (MIN_SWAP_SQRT_PRICE_X96, MAX_SWAP_SQRT_PRICE_X96),
            Self::V1 => (V1_MIN_SWAP_SQRT_PRICE_X96, V1_MAX_SWAP_SQRT_PRICE_X96),
        };
        (SqrtPrice::from_x96(min), SqrtPrice::from_x96(max))
    }

    /// The highest protocol fee a market of this build can charge: 5% on
    /// the earlier builds, 1% on `v1.0.0`, whose liquidation close reserve
    /// adds it to the fees module's `maxSwapFee()`.
    pub fn max_protocol_fee(self) -> Ratio {
        let e6 = match self {
            Self::Legacy | Self::Upgradeable => MAX_PROTOCOL_FEE,
            Self::V1 => V1_MAX_PROTOCOL_FEE,
        };
        Ratio::from_e6(e6).expect("both caps are below 100%")
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
    /// reads the first time any reader of this market asks: the pool key,
    /// whose hook names the era, then the era's pool id view (`POOL_ID()`
    /// before `v1.0.0`, `poolId()` on it). Not a block's to give, so it
    /// lives here rather than on the handle.
    ///
    /// # Errors
    ///
    /// [`ContractError::UnknownBuild`] when the hook is none the chain's
    /// deployments name; it reads nothing more and caches nothing.
    pub(super) async fn immutables(&self) -> Result<MarketImmutables> {
        {
            let cached = self.chain.immutables_cache().lock().unwrap();
            if let Some(immutables) = cached.get(&self.perp) {
                return Ok(*immutables);
            }
        }

        let provider = self.chain.provider();
        let pool_key = Perp::new(self.perp, provider).poolKey().call().await?;
        let era =
            self.chain
                .deployments()
                .era_of(pool_key.hooks)
                .ok_or(ContractError::UnknownBuild {
                    perp: self.perp,
                    hook: pool_key.hooks,
                })?;
        let pool_id = match era {
            Era::Legacy | Era::Upgradeable => {
                Perp::new(self.perp, provider).POOL_ID().call().await?
            }
            Era::V1 => v1::Perp::new(self.perp, provider).poolId().call().await?,
        };
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
            era,
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
    /// time of the call, and the tape scans that one for the whole range;
    /// a swap inside the range shows on the tape as a `ModuleSet`, and the
    /// earlier beacon's prints before it are not read (see `market_tape`).
    ///
    /// # Errors
    ///
    /// [`ContractError::UnsupportedEra`] for a `v1.0.0` market, before any
    /// module or log read: the tape decoder knows no `v1.0.0` position
    /// event yet, so its tape would drop them with no gap;
    /// [`ContractError::UnknownBuild`] for an unknown build;
    /// [`ContractError::ModuleNotRegistered`] when the perp has no beacon;
    /// the transport error from any read.
    pub async fn tape_addresses(&self) -> Result<TapeAddresses> {
        let immutables = self.immutables().await?;
        if immutables.era == Era::V1 {
            return Err(ContractError::UnsupportedEra {
                era: Era::V1,
                operation: "tape_addresses",
            }
            .into());
        }
        let modules = Perp::new(self.perp, self.chain.provider())
            .modules()
            .call()
            .await?;
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
    /// modules set, each read in the shape of the market's build.
    ///
    /// Uses the [`crate::hft::state_cache::StateCache`] for bounds and pre-v1 fees (60s TTL).
    ///
    /// # Errors
    ///
    /// [`ContractError::UnknownBuild`] for an unknown build, before any
    /// view is read; [`ContractError::ModuleNotRegistered`] for a missing
    /// fees or margin-ratio module; the transport error from any read.
    pub async fn get_config(&self) -> Result<MarketConfig> {
        let era = self.immutables().await?.era;
        let perp = Perp::new(self.perp, self.chain.provider());

        let modules = perp.modules().call().await?;
        let pool_key = perp.poolKey().call().await?;
        let pool_state = perp.poolState().call().await?;
        let ema_window = match era {
            Era::Legacy | Era::Upgradeable => ema_window_secs(perp.EMA_WINDOW().call().await?)?,
            Era::V1 => {
                let window = v1::Perp::new(self.perp, self.chain.provider())
                    .emaWindow()
                    .call()
                    .await?;
                u64::from(u24_to_u32(window))
            }
        };
        let pool_price = read_price(pool_state.ammPrice, "pool price")?;

        let fees = self.get_or_fetch_fees(era, modules.fees).await?;
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

    /// The rates a `v1.0.0` taker trade's fee bound is sized from, read now:
    /// the fees module's `maxSwapFee()` and the perp's `protocolFee()`, in
    /// one multicall after `modules()`, pinned to the same head block.
    /// Governance can change the module and the perp syncs its protocol fee.
    ///
    /// # Errors
    ///
    /// [`ContractError::UnsupportedEra`] for an earlier build, which has no
    /// fee bound, and [`ContractError::UnknownBuild`] for an unknown one,
    /// before any view is read; [`ContractError::ModuleNotRegistered`] for a
    /// missing fees module; [`ValidationError::Overflow`] for a rate past
    /// 100%; the transport error from any read.
    pub async fn get_taker_fee_rates(&self) -> Result<TakerFeeRates> {
        let era = self.immutables().await?.era;
        if era != Era::V1 {
            return Err(ContractError::UnsupportedEra {
                era,
                operation: "get_taker_fee_rates",
            }
            .into());
        }
        self.state_at_head().await?.taker_fee_rates().await
    }

    /// One position's stored row, read now, in the shape of the market's
    /// build ([`Self::era`]). Use [`crate::math::position`] functions to
    /// compute derived values (entry price, PnL, etc.).
    ///
    /// # Errors
    ///
    /// [`ContractError::PositionNotFound`] for an id never minted or
    /// closed, which every build reports as an empty row;
    /// [`ContractError::UnknownBuild`] for an unknown build, before the
    /// row is read; the transport error from any read.
    pub async fn get_position(&self, pos_id: U256) -> Result<PositionRow> {
        let provider = self.chain.provider();
        let row = match self.immutables().await?.era {
            Era::Legacy | Era::Upgradeable => PositionRow::from(
                Perp::new(self.perp, provider)
                    .positions(pos_id)
                    .call()
                    .await?,
            ),
            Era::V1 => {
                return self
                    .v1_position(pos_id, Default::default())
                    .await?
                    .ok_or_else(|| ContractError::PositionNotFound { pos_id }.into());
            }
        };
        row.held()
            .ok_or_else(|| ContractError::PositionNotFound { pos_id }.into())
    }

    /// Every active position id held by `owner`, read now: each id in
    /// `1..nextPosId` asked for its holder, by the market's build.
    ///
    /// The two earlier builds' positions are NFTs: `ownerOf` reverts for a
    /// burned or never-minted token, and that id is skipped. A `v1.0.0`
    /// market has no NFT and no `ownerOf`: its `positionOwner` answers the
    /// zero address for an inactive id, which is skipped as
    /// [`StateAt::owners`](super::StateAt::owners) skips it (so the zero
    /// address holds nothing), and it never reverts, so any failure there
    /// is an error, never a skipped id.
    ///
    /// **Note:** This is O(n) in total positions ever minted. For high-throughput
    /// use cases, prefer [`StateAt::owners`](super::StateAt::owners), one
    /// row multicall per 500 ids.
    ///
    /// # Errors
    ///
    /// [`ContractError::UnknownBuild`] for an unknown build, before the
    /// counter is read; [`ValidationError::Overflow`] for a counter past
    /// `u64`; the transport error from any read.
    pub async fn get_positions_by_owner(&self, owner: Address) -> Result<Vec<U256>> {
        let era = self.immutables().await?.era;
        let provider = self.chain.provider();
        // `nextPosId()` has one selector and one return on every build.
        let next_pos_id: U256 = Perp::new(self.perp, provider).nextPosId().call().await?;

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
            let holder = match era {
                Era::Legacy | Era::Upgradeable => self.nft_owner(pos_id).await?,
                Era::V1 => {
                    let holder = v1::Perp::new(self.perp, provider)
                        .positionOwner(pos_id)
                        .call()
                        .await?;
                    (holder != Address::ZERO).then_some(holder)
                }
            };
            if holder == Some(owner) {
                owned.push(pos_id);
            }
        }

        Ok(owned)
    }

    /// The holder of an earlier build's position NFT, or `None` for a
    /// burned or never-minted token.
    ///
    /// `ownerOf` reverts for those. How a revert surfaces is
    /// provider-dependent: some decode into contract errors, others wrap
    /// the raw JSON-RPC error response (code 3, revert data attached) as a
    /// transport error. Classify by revert data, and only propagate
    /// genuine transport failures so network errors aren't silently
    /// ignored.
    async fn nft_owner(&self, pos_id: U256) -> Result<Option<Address>> {
        match Perp::new(self.perp, self.chain.provider())
            .ownerOf(pos_id)
            .call()
            .await
        {
            Ok(holder) => Ok(Some(holder)),
            Err(alloy::contract::Error::TransportError(e))
                if e.as_error_resp()
                    .and_then(|resp| resp.as_revert_data())
                    .is_none() =>
            {
                Err(alloy::contract::Error::TransportError(e).into())
            }
            Err(_) => Ok(None),
        }
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

    /// The market's configuration and its state at the lagged block.
    ///
    /// Resolves the lagged snapshot block (see
    /// [`SNAPSHOT_BLOCK_LAG`](crate::constants::SNAPSHOT_BLOCK_LAG)) from the
    /// node's header, batches the `Perp`'s views at it, then reads `index()`
    /// on the beacon the batch named and the EMA slot together at the same
    /// block: every field of the snapshot is from that one block, which it
    /// carries. The stored pair is in slot 11 on earlier builds and slot
    /// 23 on v1. On v1 the pricing module prices the advanced pair at
    /// that block and the fee module returns its configuration at the same hash.
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
        let era = self.immutables().await?.era;
        if era == Era::V1 {
            return self.v1_snapshot().await;
        }
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
        let fees = self.get_or_fetch_fees(era, modules.fees).await?;
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

    /// [`Self::get_snapshot`] on a `v1.0.0` market: the same one multicall
    /// in its own shapes (`emaWindow()` for `EMA_WINDOW()`), the beacon's
    /// index and the EMAs from slot 23, then the mark from the market's
    /// pricing module by `eth_call`, every read at the lagged block.
    async fn v1_snapshot(&self) -> Result<(MarketConfig, MarketSnapshot)> {
        let perp = v1::Perp::new(self.perp, self.chain.provider());
        let state = self.state().await?;
        let block = state.block();
        let (modules, pool_key, pool_state, rates, oi, ema_window) = self
            .chain
            .multicall_at(state.id())
            .add(perp.modules())
            .add(perp.poolKey())
            .add(perp.poolState())
            .add(perp.rates())
            .add(perp.openInterest())
            .add(perp.emaWindow())
            .aggregate()
            .await
            .map_err(|e| pinned_read_error(multicall_error(e), block.number))?;

        let pool_price = read_price(pool_state.ammPrice, "pool price")?;
        let funding_rate_daily = funding_per_day_to_f64(rates.fundingPerDay);
        let views = V1Views {
            modules,
            pool_state,
            last_touch: rates.lastTouch.to::<u64>(),
            ema_window: u64::from(u24_to_u32(ema_window)),
        };
        let (mark, stored_emas) = state.v1_mark_from(&views).await?;
        let modules = &views.modules;

        let fees = self
            .fetch_v1_fees(modules.fees, state.id())
            .await
            .map_err(|e| pinned_read_error(e, block.number))?;
        let bounds = self.get_or_fetch_bounds(modules.marginRatios).await?;
        let config = MarketConfig {
            perp: self.perp,
            tick_spacing: i24_to_i32(pool_key.tickSpacing),
            ema_window: views.ema_window,
            pool_price,
            beacon: modules.beacon,
            bounds,
            fees,
        };
        let snapshot = MarketSnapshot {
            block,
            pool_price,
            index_price: mark.index,
            mark: mark.price,
            emas: Emas::stored(stored_emas, views.last_touch),
            funding_rate_daily,
            open_interest: OpenInterest::new(PerpAtoms::new(oi.long), PerpAtoms::new(oi.short)),
        };
        Ok((config, snapshot))
    }

    // ── Cache helpers ───────────────────────────────────────────────

    /// Get fees from cache or fetch them from the fees module at
    /// `fees_addr`, in the shape of a market of build `era`.
    async fn get_or_fetch_fees(&self, era: Era, fees_addr: Address) -> Result<Fees> {
        if era == Era::V1 {
            return self.fetch_v1_fees(fees_addr, BlockId::latest()).await;
        }
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
            lp_fee: LpFee::Fixed(Ratio::from_e6(u24_to_u32(fee_result.lpFee))?),
            liquidation_fee: Ratio::from_e6(u24_to_u32(liquidation_fee))?,
        })
    }

    /// Fetch a `v1.0.0` fees module's rates in one multicall at the requested block:
    /// `cFee()`, `insFee()`, `lpFeeConfig()`, `maxSwapFee()` and
    /// `liqFee()`. The module has no zero-argument `fees()`; its
    /// three-argument one is a segment's quote.
    async fn fetch_v1_fees(&self, fees_addr: Address, block: BlockId) -> Result<Fees> {
        let module = v1::IFees::new(
            registered_module(fees_addr, "IFees")?,
            self.chain.provider(),
        );
        let (c_fee, ins_fee, curve, max_swap_fee, liq_fee) = self
            .chain
            .multicall_at(block)
            .add(module.cFee())
            .add(module.insFee())
            .add(module.lpFeeConfig())
            .add(module.maxSwapFee())
            .add(module.liqFee())
            .aggregate()
            .await
            .map_err(multicall_error)?;
        let ratio = |e6| Ratio::from_e6(u24_to_u32(e6));
        let max_swap_fee = u32::try_from(max_swap_fee)
            .map_err(|_| ValidationError::Overflow {
                context: "maxSwapFee exceeds uint24".into(),
            })
            .and_then(Ratio::from_e6)?;
        Ok(Fees {
            creator_fee: ratio(c_fee)?,
            insurance_fee: ratio(ins_fee)?,
            lp_fee: LpFee::Curve(LpFeeCurve {
                start_fee: ratio(curve.startFee)?,
                target_fee: ratio(curve.targetFee)?,
                liquidity_decay: LUnits::new(curve.liquidityDecay),
                volatility_scaler: ratio(curve.volatilityScaler)?,
                max_fee_multiplier: ratio(curve.maxFeeMultiplier)?,
                max_swap_fee,
            }),
            liquidation_fee: ratio(liq_fee)?,
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
    pub async fn get_usdc_balance(&self) -> Result<UsdcAtoms> {
        self.chain().balance_of(self.address).await
    }
}

/// Characterisation of the reads as deployed: each test pins what a read
/// asks the chain, what it makes of the answer, and what it keeps.
#[cfg(test)]
mod tests {
    use alloy::primitives::Uint;
    use alloy::sol_types::SolCall;

    use super::*;
    use crate::client::Balances;
    use crate::client::mock::{self, BEACON, Rpc, e6, failed_row, ok_row, returns, x96};
    use crate::constants::SNAPSHOT_BLOCK_LAG;
    use crate::contracts::{IBeacon, IERC20, IMulticall3, Modules, Rates};
    use crate::convert::pack_balance_delta;
    use crate::errors::PerpCityError;
    use crate::math::BlockContext;
    use crate::math::pricing::PricePair;
    use crate::units::{PerpDelta, Side, UsdcDelta};

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
            lp_fee: LpFee::Fixed(ratio(3_000)),
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

    /// The signer's USDC balance, exact to the atom, and cached in the fast
    /// layer alongside the prices.
    #[tokio::test]
    async fn usdc_balance_is_exact_and_cached() {
        let (client, rpc) = mock::client();
        rpc.call::<IERC20::balanceOfCall>(&U256::from(1_234_567u32));

        let balance = UsdcAtoms::new(1_234_567);
        assert_eq!(client.get_usdc_balance().await.unwrap(), balance);
        assert_eq!(
            client.get_usdc_balance().await.unwrap(),
            balance,
            "cache hit"
        );

        client.chain().invalidate_fast_cache();
        rpc.call::<IERC20::balanceOfCall>(&U256::ZERO);
        assert_eq!(client.get_usdc_balance().await.unwrap(), UsdcAtoms::ZERO);
        assert!(rpc.is_drained());
    }

    /// A balance beyond `u128` is an overflow, not a saturated number.
    #[tokio::test]
    async fn usdc_balance_beyond_u128_is_an_overflow() {
        let (client, rpc) = mock::client();
        rpc.call::<IERC20::balanceOfCall>(&(U256::from(u128::MAX) + U256::from(1u8)));

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

    /// `positions(id)` comes back as a [`PositionRow`] of its stored
    /// fields, untouched.
    #[tokio::test]
    async fn a_position_is_a_position_row_of_its_stored_fields() {
        let (client, rpc) = mock::client();
        rpc.immutables(Era::Legacy);
        rpc.call::<Perp::positionsCall>(&mock::position(1_000_000));

        let position = client.market().get_position(U256::from(7u8)).await.unwrap();
        assert_eq!(position.margin, UsdcAtoms::new(1_000_000));
        assert!(position.perp == PerpDelta::ZERO && position.usd == UsdcDelta::ZERO);
        assert!(rpc.is_drained());
    }

    /// The contract answers a burned or never-minted id with an all-zero
    /// struct rather than a revert; the read names it.
    #[tokio::test]
    async fn an_all_zero_position_is_not_found() {
        let (client, rpc) = mock::client();
        rpc.immutables(Era::Legacy);
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
        rpc.immutables(Era::Legacy);
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
        rpc.immutables(Era::Legacy);
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
            rpc.immutables(Era::Legacy);
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
        rpc.immutables(Era::Legacy);
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

    // ── Positions on each build ───────────────────────────────────────

    /// A `v1.0.0` market's row is three words, read through its binding:
    /// no stored ratios; a closed taker's deleted row is `PositionNotFound`.
    #[tokio::test]
    async fn get_position_reads_the_v1_row() {
        let (client, rpc) = mock::client();
        rpc.immutables(Era::V1);
        rpc.aggregate3(vec![
            ok_row(returns::<v1::Perp::positionsCall>(&v1::Position {
                delta: pack_balance_delta(-400_000, 9_000_000),
                margin: 10_000_000,
                lastCumlFundingX96: alloy::primitives::I256::ZERO,
            })),
            ok_row(returns::<v1::Perp::positionOwnerCall>(&BEACON)),
        ]);
        let row = client.market().get_position(U256::from(2u8)).await.unwrap();
        assert_eq!(
            (row.perp, row.usd, row.margin, row.stored_margin_ratios),
            (
                PerpDelta::new(-400_000),
                UsdcDelta::new(9_000_000),
                UsdcAtoms::new(10_000_000),
                None
            )
        );

        rpc.aggregate3(vec![
            ok_row(returns::<v1::Perp::positionsCall>(&v1::Position {
                delta: alloy::primitives::I256::ZERO,
                margin: 0,
                lastCumlFundingX96: alloy::primitives::I256::ZERO,
            })),
            ok_row(returns::<v1::Perp::positionOwnerCall>(&Address::ZERO)),
        ]);
        let err = client
            .market()
            .get_position(U256::from(3u8))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::PositionNotFound { pos_id })
                    if pos_id == U256::from(3u8)
            ),
            "{err}"
        );
        assert!(rpc.is_drained(), "poolKey, poolId, then one row per read");
    }

    /// An unknown build reads its pool key and nothing more.
    #[tokio::test]
    async fn get_position_refuses_an_unknown_build() {
        let (client, rpc) = mock::client();
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key_with_hook(30, Address::repeat_byte(0x0e)));
        let err = client.market().get_position(U256::ONE).await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::UnknownBuild { .. })
            ),
            "{err}"
        );
        assert!(rpc.is_drained());
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key_with_hook(30, Address::repeat_byte(0x0e)));
        let err = client
            .market()
            .get_positions_by_owner(Address::repeat_byte(0xaa))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::UnknownBuild { .. })
            ),
            "{err}"
        );
        assert!(rpc.is_drained(), "no nextPosId, no owner read");
    }

    /// Map row 13, the old way. `v1.0.0` has no `ownerOf` and no fallback,
    /// so every `ownerOf` reverts with empty data. Read on the earlier
    /// builds' walk, that is silent where the provider attaches the empty
    /// data (each id is taken for a burned token, and the owner's position
    /// is lost) and loud where it attaches none.
    #[tokio::test]
    async fn the_owner_of_walk_on_a_v1_answer_is_silent_or_loud_by_provider() {
        let owner = Address::repeat_byte(0xaa);
        let (client, rpc) = mock::client();
        rpc.immutables(Era::Upgradeable);
        rpc.call::<Perp::nextPosIdCall>(&U256::from(3u8));
        rpc.reverts(&[]);
        rpc.reverts(&[]);
        let owned = client.market().get_positions_by_owner(owner).await.unwrap();
        assert!(owned.is_empty(), "silent: the empty revert reads as burned");
        assert!(rpc.is_drained());

        rpc.call::<Perp::nextPosIdCall>(&U256::from(3u8));
        rpc.fails("execution reverted");
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
            "loud: no revert data is a transport error: {err}"
        );
        assert!(rpc.is_drained());
        assert!(
            rpc.selectors()
                .iter()
                .filter(|s| **s == Perp::ownerOfCall::SELECTOR)
                .count()
                == 3
        );
    }

    /// Map row 13, the new way. A `v1.0.0` market's walk asks
    /// `positionOwner`, never `ownerOf`: the zero address is an inactive
    /// id (a closed taker), and the owner's position is found. The empty
    /// revert that the old walk took for a burned token is an error here,
    /// since `positionOwner` never reverts.
    #[tokio::test]
    async fn positions_by_owner_on_v1_asks_position_owner() {
        let owner = Address::repeat_byte(0xaa);
        let (client, rpc) = mock::client();
        rpc.immutables(Era::V1);
        rpc.call::<v1::Perp::nextPosIdCall>(&U256::from(4u8));
        rpc.call::<v1::Perp::positionOwnerCall>(&owner);
        rpc.call::<v1::Perp::positionOwnerCall>(&Address::ZERO);
        rpc.call::<v1::Perp::positionOwnerCall>(&Address::repeat_byte(0xbb));
        let owned = client.market().get_positions_by_owner(owner).await.unwrap();
        assert_eq!(owned, vec![U256::ONE]);
        let selectors = rpc.selectors();
        assert_eq!(
            selectors,
            [
                Perp::poolKeyCall::SELECTOR,
                v1::Perp::poolIdCall::SELECTOR,
                v1::Perp::nextPosIdCall::SELECTOR,
                v1::Perp::positionOwnerCall::SELECTOR,
                v1::Perp::positionOwnerCall::SELECTOR,
                v1::Perp::positionOwnerCall::SELECTOR,
            ]
        );
        assert!(!selectors.contains(&Perp::ownerOfCall::SELECTOR));
        assert!(rpc.is_drained());

        rpc.call::<v1::Perp::nextPosIdCall>(&U256::from(2u8));
        rpc.reverts(&[]);
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
        assert!(rpc.is_drained());
    }

    /// On `v1.0.0` the zero address is an inactive id, never a holder: a
    /// walk for `Address::ZERO` finds no closed id, and a walk for the
    /// holder finds only its active one.
    #[tokio::test]
    async fn positions_by_owner_on_v1_never_finds_a_closed_id() {
        let holder = Address::repeat_byte(0xaa);
        let (client, rpc) = mock::client();
        rpc.immutables(Era::V1);
        for owner in [Address::ZERO, holder] {
            rpc.call::<v1::Perp::nextPosIdCall>(&U256::from(3u8));
            rpc.call::<v1::Perp::positionOwnerCall>(&holder);
            rpc.call::<v1::Perp::positionOwnerCall>(&Address::ZERO);
            let owned = client.market().get_positions_by_owner(owner).await.unwrap();
            let expected = if owner == holder {
                vec![U256::ONE]
            } else {
                vec![]
            };
            assert_eq!(owned, expected, "{owner}");
            assert!(rpc.is_drained(), "{owner}");
        }
    }

    /// The earlier builds keep the `ownerOf` walk.
    #[tokio::test]
    async fn positions_by_owner_on_the_earlier_builds_asks_owner_of() {
        let owner = Address::repeat_byte(0xaa);
        for era in [Era::Legacy, Era::Upgradeable] {
            let (client, rpc) = mock::client();
            rpc.immutables(era);
            rpc.call::<Perp::nextPosIdCall>(&U256::from(2u8));
            rpc.call::<Perp::ownerOfCall>(&owner);
            let owned = client.market().get_positions_by_owner(owner).await.unwrap();
            assert_eq!(owned, vec![U256::ONE]);
            assert_eq!(
                rpc.selectors()[2..],
                [Perp::nextPosIdCall::SELECTOR, Perp::ownerOfCall::SELECTOR]
            );
            assert!(rpc.is_drained());
        }
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
        rpc.immutables(Era::Legacy);
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
        rpc.immutables(Era::Legacy);
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
        rpc.immutables(Era::Legacy);
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
        rpc.immutables(Era::Legacy);
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
        let expected = [
            Balances {
                usdc: UsdcAtoms::new(1_000_000),
                eth_wei: U256::from(5u8),
            },
            Balances {
                usdc: UsdcAtoms::new(2_500_000),
                eth_wei: U256::ZERO,
            },
        ];
        assert_eq!(balances, expected);
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
        rpc.immutables(Era::Legacy);
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
        rpc.immutables(Era::Legacy);
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
        rpc.immutables(Era::Legacy);
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
        rpc.immutables(Era::Legacy);
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
        rpc.immutables(Era::Legacy);
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
                price: Price::from_x96(one),
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
        rpc.immutables(Era::Legacy);
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
