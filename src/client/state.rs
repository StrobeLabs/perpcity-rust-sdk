//! Reads of the market at one block, [`StateAt`].
//!
//! The counterpart of [`queries`](super::queries): there, every read is
//! independently current; here, a handle resolves one block and every read
//! on it is pinned to that block's hash. Two values that must agree — open
//! interest against capacity, a position against the mark — are read
//! through one handle, and agree by construction.

use std::collections::BTreeMap;
use std::sync::Arc;

use alloy::eips::BlockId;
use alloy::primitives::{Address, B256, U256};
use alloy::providers::MulticallError;
use alloy::sol_types::SolCall;
use alloy::transports::{RpcError, TransportError};
use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};

use crate::constants::{
    MAX_SWAP_SQRT_PRICE_X96, MAX_TICK, MIN_SWAP_SQRT_PRICE_X96, MIN_TICK, MULTICALL3,
};
use crate::contracts::{
    IERC20, IMarginRatios, IMulticall3, IPoolManagerState, IPriceImpact, Modules, Perp, Position,
};
use crate::convert::usdc_from_atoms;
use crate::errors::{ContractError, PerpCityError, Result, ValidationError};
use crate::events::CumulativesInfo;
use crate::math::BlockContext;
use crate::math::capacity::MarketCapacity;
use crate::math::pricing::{Emas, Mark};
use crate::math::range::{MakerBand, TickRange};
use crate::math::swap::{PoolSnapshot, TickLiquidity, active_liquidity};
use crate::storage::{v4_tick_bitmap_slot, v4_tick_slot};
use crate::units::{
    FundingRate, LDelta, LUnits, PerSide, Price, Ratio, SqrtPrice, UsdcAtoms, UtilizationRate,
};

use super::market::MarketReader;
use super::queries::{MarketImmutables, multicall_error, registered_module};
use super::{i24_to_i32, u24_to_u32};

/// Maximum position ids per multicall inside a batched row read.
///
/// [`StateAt::positions`] and the maker-equity batch chunk larger inputs
/// internally at this size (every chunk still pins to the handle's block),
/// keeping each row multicall and slot read inside RPC response-size and
/// calldata limits. Exposed so callers sizing their own sweeps can align
/// with it.
pub const MAX_ROW_BATCH: usize = 500;

/// Concurrency bound for the chunks of a batch larger than
/// [`MAX_ROW_BATCH`]: each chunk is at least one multicall, so a few in
/// flight saturate a shared endpoint's fair share without flooding it.
pub(super) const CHUNK_READ_CONCURRENCY: usize = 4;

/// One market's storage, every read pinned to one block.
///
/// The handle is the block: it resolves the header once and pins each
/// read to that hash, so values read through one handle cannot come from
/// different blocks. [`MarketReader::state`] pins the lagged snapshot
/// block the other pinned reads use; [`MarketReader::state_at`] pins a
/// block the caller names, which past a recent window needs an archive
/// endpoint.
///
/// Owned and cheap to clone (the reader is Arc-backed), so a fan-out over
/// thousands of positions can move it into tasks. It is the state half of
/// what [`History`](crate::history::History) is for logs.
#[derive(Clone, Debug)]
pub struct StateAt {
    market: MarketReader,
    block: BlockContext,
}

/// One side's margin-ratio thresholds.
///
/// Each is the contract's own `uint24` at 1e6, so the integers here are the
/// module's values rather than a fraction recovered from one; `fraction()`
/// on any of them is the number a person reads.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MarginRatioTriple {
    /// Minimum equity over value to open or increase a position.
    pub init: Ratio,
    /// Equity over value below which the position is liquidatable — the
    /// ratio a position opened now stores as its own.
    pub liquidation: Ratio,
    /// Equity over value below which the position can be backstopped.
    pub backstop: Ratio,
}

/// The market's `IMarginRatios` module: maker and taker thresholds.
///
/// These are the module's CURRENT values, applied to positions opened from
/// now on; an open position keeps the liquidation ratio stored on it at
/// open (`positions(id).liqMarginRatio`, surfaced by
/// [`MakerEquityBreakdown::liquidation_margin_ratio`](crate::MakerEquityBreakdown::liquidation_margin_ratio)).
/// Read with [`MarketReader::get_margin_ratios`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MarginRatios {
    /// Maker (LP) thresholds.
    pub maker: MarginRatioTriple,
    /// Taker thresholds.
    pub taker: MarginRatioTriple,
}

/// The market's own solvency books, in the chain's units: the contract's
/// `SolvencyState`.
///
/// `total_margin` moves only when real USDC enters or leaves, so it is
/// the honest upper bound on what positions may collectively claim. Both
/// are atoms, because the figures they are compared against — the sum of
/// every position's margin, the USDC the contract holds, the same books
/// rebuilt from the tape — are exact, and a comparison to the cent needs
/// both sides exact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SolvencyState {
    /// Insolvency the contract has recognised and booked.
    pub bad_debt: UsdcAtoms,
    /// Margin the contract believes it holds.
    pub total_margin: UsdcAtoms,
}

/// The rates the last touch set, as the contract's `rates()` returns them
/// and as `RatesAndEmasRefreshed` emits them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketRates {
    /// Daily funding rate; positive when longs pay shorts.
    pub funding_per_day: FundingRate,
    /// Utilization fee per day, per side.
    pub util_fee_per_day: PerSide<UtilizationRate>,
    /// Unix timestamp of the touch that set them.
    pub last_touch: u64,
}

/// One id's row in a batched read: every id passed to [`StateAt::positions`]
/// comes back as exactly one of these, in input order.
#[derive(Debug)]
pub struct RowOutcome<T> {
    /// The requested position id.
    pub pos_id: U256,
    /// What the read produced for it: `Ok(Some)` the row, `Ok(None)` an id
    /// the contract holds no row for, `Err` when that id's read or decode
    /// failed. The rest of the batch is unaffected by an `Err`; retry it
    /// exactly when [`PerpCityError::is_transient`] says so.
    pub row: Result<Option<T>>,
}

/// A failure of one chunk's shared read (its row multicall, or a storage
/// batch over its rows), reshaped so every id in the chunk can carry it as
/// its own error. A transport cause is kept behind an `Arc` so the shared
/// error stays retryable ([`PerpCityError::is_transient`]) on every
/// affected id; the two answers a node gives about the block itself keep
/// their typed, block-naming variants.
pub(super) enum ChunkReadFailure {
    Storage {
        context: String,
        source: Option<Arc<TransportError>>,
    },
    BlockUnavailable {
        number: u64,
    },
    StateUnavailable {
        number: u64,
    },
}

impl ChunkReadFailure {
    /// `error`, as `what` failing, ready to fan out.
    pub(super) fn new(what: &str, error: PerpCityError) -> Self {
        let storage = |context, source| Self::Storage { context, source };
        match error {
            PerpCityError::Rpc(e)
            | PerpCityError::Abi(alloy::contract::Error::TransportError(e)) => {
                storage(what.to_string(), Some(Arc::new(e)))
            }
            PerpCityError::Contract(ContractError::StorageReadFailed { context, source }) => {
                storage(context, source)
            }
            PerpCityError::Contract(ContractError::BlockUnavailable { number }) => {
                Self::BlockUnavailable { number }
            }
            PerpCityError::Contract(ContractError::StateUnavailable { number }) => {
                Self::StateUnavailable { number }
            }
            other => storage(format!("{what}: {other}"), None),
        }
    }

    /// One id's copy of the failure.
    pub(super) fn error(&self) -> PerpCityError {
        match self {
            Self::Storage { context, source } => ContractError::StorageReadFailed {
                context: context.clone(),
                source: source.clone(),
            },
            Self::BlockUnavailable { number } => {
                ContractError::BlockUnavailable { number: *number }
            }
            Self::StateUnavailable { number } => {
                ContractError::StateUnavailable { number: *number }
            }
        }
        .into()
    }
}

impl MarketReader {
    /// State reads pinned to the lagged, reorg-safe snapshot block (see
    /// [`SNAPSHOT_BLOCK_LAG`](crate::constants::SNAPSHOT_BLOCK_LAG)).
    ///
    /// # Errors
    ///
    /// [`ContractError::BlockUnavailable`] when the serving replica is
    /// missing the pinned header.
    pub async fn state(&self) -> Result<StateAt> {
        let (block, _) = self.chain.lagged_snapshot_block().await?;
        Ok(StateAt {
            market: self.clone(),
            block,
        })
    }

    /// State reads pinned to block `number`.
    ///
    /// The header is resolved here; the state behind it is only checked
    /// by the reads. A full (non-archive) endpoint keeps every header but
    /// prunes old state, so it hands out the handle and then fails each
    /// read with [`ContractError::StateUnavailable`], which is not
    /// transient: the fix is an archive endpoint, not a retry.
    ///
    /// # Errors
    ///
    /// [`ContractError::BlockUnavailable`] when the endpoint does not
    /// serve that header.
    pub async fn state_at(&self, number: u64) -> Result<StateAt> {
        let (block, _) = self.chain.block_at(number).await?;
        Ok(StateAt {
            market: self.clone(),
            block,
        })
    }

    // ── One pinned read, on a fresh lagged handle ─────────────────────
    //
    // Each of these is `state().await?.<read>()`: the single-read
    // convenience. Anything that reads twice and needs the values to
    // agree takes `state()` once and reads on it.

    /// [`StateAt::capacity`] at the lagged snapshot block.
    pub async fn get_capacity(&self) -> Result<MarketCapacity> {
        self.state().await?.capacity().await
    }

    /// [`StateAt::margin_ratios`] at the lagged snapshot block.
    pub async fn get_margin_ratios(&self) -> Result<MarginRatios> {
        self.state().await?.margin_ratios().await
    }

    /// [`StateAt::pool`] at the lagged snapshot block.
    ///
    /// The deployment immutables are read first: they are not the block's
    /// to give, and a market whose immutables are rejected never resolves
    /// a block at all.
    pub async fn get_pool_snapshot(&self) -> Result<PoolSnapshot> {
        self.immutables().await?;
        self.state().await?.pool().await
    }
}

impl StateAt {
    /// The block every read on this handle is pinned to.
    pub fn block(&self) -> BlockContext {
        self.block
    }

    /// The market being read.
    pub fn market(&self) -> &MarketReader {
        &self.market
    }

    pub(super) fn id(&self) -> BlockId {
        BlockId::hash(self.block.hash)
    }

    /// A read's failure, typed for this handle's block by
    /// [`pinned_read_error`].
    pub(super) fn read_error(&self, error: alloy::contract::Error) -> PerpCityError {
        pinned_read_error(error.into(), self.block.number)
    }

    /// The market's solvency books.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] if a figure does not fit `i128` — a
    /// broken read, not a balance.
    pub async fn solvency(&self) -> Result<SolvencyState> {
        let perp = Perp::new(self.market.perp, self.market.chain.provider());
        let state = perp
            .solvencyState()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        Ok(SolvencyState {
            bad_debt: UsdcAtoms::new(state.badDebt),
            total_margin: UsdcAtoms::new(state.totalMargin),
        })
    }

    /// Positions ever minted: ids `1..next_pos_id()` are the range a scan
    /// of the market covers.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] if the count does not fit `u64` — a
    /// broken read, since no market has minted that many.
    pub async fn next_pos_id(&self) -> Result<u64> {
        let perp = Perp::new(self.market.perp, self.market.chain.provider());
        let next = perp
            .nextPosId()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        u64::try_from(next).map_err(|_| {
            ValidationError::Overflow {
                context: format!("nextPosId {next} exceeds u64"),
            }
            .into()
        })
    }

    /// One position's raw contract state, or `None` for an id that was
    /// never minted or has been closed (the contract reports both as an
    /// empty struct).
    ///
    /// `None` rather than an error because this is the read for
    /// enumerating `1..next_pos_id()`, where most ids are closed;
    /// [`MarketReader::get_position`] asks about a position expected to
    /// exist and fails with `PositionNotFound` instead.
    pub async fn position(&self, pos_id: U256) -> Result<Option<Position>> {
        let perp = Perp::new(self.market.perp, self.market.chain.provider());
        let position = perp
            .positions(pos_id)
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        Ok((position.margin != 0 || !position.delta.is_zero()).then_some(position))
    }

    /// The raw contract state of many positions, in one row multicall per
    /// [`MAX_ROW_BATCH`] ids, all at this block.
    ///
    /// Exactly one [`RowOutcome`] per input id, in input order, with
    /// [`Self::position`]'s reading of each row: `None` for an id that was
    /// never minted or has been closed. A row that reverts or does not
    /// decode fails alone; a chunk whose multicall fails marks every id of
    /// that chunk failed with the shared cause, and the other chunks stand.
    /// Nothing fails the batch, so a sweep over `1..next_pos_id()` always
    /// gets an answer for every id.
    pub async fn positions(&self, pos_ids: &[U256]) -> Vec<RowOutcome<Position>> {
        self.rows(
            pos_ids,
            |pos_id| [Perp::positionsCall { posId: pos_id }.abi_encode()],
            |pos_id, [row]| {
                let position = decode_row::<Perp::positionsCall>(pos_id, row, "position")?;
                Ok((position.margin != 0 || !position.delta.is_zero()).then_some(position))
            },
        )
        .await
    }

    /// A batched row read: `pos_ids` in chunks of [`MAX_ROW_BATCH`], each
    /// chunk one `aggregate3` at this block with `CALLS` views per id, each
    /// row allowed to fail on its own, and `decode` turning one id's rows
    /// into its value. Chunks read concurrently, bounded, and come back in
    /// input order.
    pub(super) async fn rows<T, const CALLS: usize>(
        &self,
        pos_ids: &[U256],
        encode: impl Fn(U256) -> [Vec<u8>; CALLS] + Sync,
        decode: impl Fn(U256, &[IMulticall3::Result; CALLS]) -> Result<Option<T>> + Sync,
    ) -> Vec<RowOutcome<T>> {
        // Collected into a Vec first so the returned future's Send bound is
        // provable from the concrete future type, not the borrowing
        // iterator adapter.
        let (encode, decode) = (&encode, &decode);
        let chunk_reads: Vec<_> = pos_ids
            .chunks(MAX_ROW_BATCH)
            .map(|chunk| async move {
                match self.chunk_rows(chunk, encode).await {
                    Ok(rows) => chunk
                        .iter()
                        .zip(rows.as_chunks::<CALLS>().0)
                        .map(|(&pos_id, rows)| RowOutcome {
                            pos_id,
                            row: decode(pos_id, rows).inspect_err(|e| {
                                tracing::debug!(%pos_id, error = %e, "position row failed");
                            }),
                        })
                        .collect::<Vec<_>>(),
                    Err(e) => {
                        tracing::debug!(ids = chunk.len(), error = %e, "row multicall failed");
                        let failure = ChunkReadFailure::new("row multicall", e);
                        chunk
                            .iter()
                            .map(|&pos_id| RowOutcome {
                                pos_id,
                                row: Err(failure.error()),
                            })
                            .collect()
                    }
                }
            })
            .collect();
        stream::iter(chunk_reads)
            .buffered(CHUNK_READ_CONCURRENCY)
            .concat()
            .await
    }

    /// One chunk's rows: `CALLS` per id, in one `aggregate3` pinned here.
    async fn chunk_rows<const CALLS: usize>(
        &self,
        pos_ids: &[U256],
        encode: impl Fn(U256) -> [Vec<u8>; CALLS],
    ) -> Result<Vec<IMulticall3::Result>> {
        let calls = pos_ids
            .iter()
            .flat_map(|&pos_id| encode(pos_id))
            .map(|calldata| IMulticall3::Call3 {
                target: self.market.perp,
                allowFailure: true,
                callData: calldata.into(),
            })
            .collect();
        let rows = IMulticall3::new(MULTICALL3, self.market.chain.provider())
            .aggregate3(calls)
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        if rows.len() != CALLS * pos_ids.len() {
            return Err(ContractError::MulticallFailed {
                reason: format!(
                    "row multicall returned {} results, expected {}",
                    rows.len(),
                    CALLS * pos_ids.len()
                ),
            }
            .into());
        }
        Ok(rows)
    }

    /// One position's band, or `None` if it holds no liquidity there: a
    /// taker, or a maker whose liquidity is gone.
    ///
    /// The chain's ticks pass through [`TickRange::new`], so this is where
    /// a malformed range would fail — never the math downstream.
    pub async fn maker_band(&self, pos_id: U256) -> Result<Option<MakerBand>> {
        let perp = Perp::new(self.market.perp, self.market.chain.provider());
        let maker = perp
            .makerDetails(pos_id)
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        if maker.liquidity == 0 {
            return Ok(None);
        }
        let range = TickRange::new(i24_to_i32(maker.tickLower), i24_to_i32(maker.tickUpper))?;
        Ok(Some(MakerBand::new(range, LUnits::new(maker.liquidity))))
    }

    /// [`Self::maker_band`] for many ids, in one row multicall per
    /// [`MAX_ROW_BATCH`] ids, all at this block: exactly one
    /// [`RowOutcome`] per id, `Ok(None)` for an id that holds no liquidity.
    pub async fn maker_bands(&self, pos_ids: &[U256]) -> Vec<RowOutcome<MakerBand>> {
        self.rows(
            pos_ids,
            |pos_id| [Perp::makerDetailsCall { posId: pos_id }.abi_encode()],
            |pos_id, [row]| {
                let maker = decode_row::<Perp::makerDetailsCall>(pos_id, row, "maker details")?;
                if maker.liquidity == 0 {
                    return Ok(None);
                }
                let range =
                    TickRange::new(i24_to_i32(maker.tickLower), i24_to_i32(maker.tickUpper))?;
                Ok(Some(MakerBand::new(range, LUnits::new(maker.liquidity))))
            },
        )
        .await
    }

    /// The module in force for each kind at this block: `modules()`.
    pub async fn modules(&self) -> Result<Modules> {
        Perp::new(self.market.perp, self.market.chain.provider())
            .modules()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))
    }

    /// The rates the last touch set at this block: `rates()`.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] if the funding rate does not fit
    /// `i128`, which an `int88` always does.
    pub async fn rates(&self) -> Result<MarketRates> {
        let rates = Perp::new(self.market.perp, self.market.chain.provider())
            .rates()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let funding =
            i128::try_from(rates.fundingPerDay).map_err(|_| ValidationError::Overflow {
                context: "funding per day".into(),
            })?;
        Ok(MarketRates {
            funding_per_day: FundingRate::from_wad(funding),
            util_fee_per_day: PerSide::new(
                UtilizationRate::from_wad(rates.longUtilFeePerDay),
                UtilizationRate::from_wad(rates.shortUtilFeePerDay),
            ),
            last_touch: rates.lastTouch.to::<u64>(),
        })
    }

    /// The market's accumulators at this block: `cumulatives()`, as the
    /// `CumulativesAccrued` event carries them.
    pub async fn cumulatives(&self) -> Result<CumulativesInfo> {
        Perp::new(self.market.perp, self.market.chain.provider())
            .cumulatives()
            .block(self.id())
            .call()
            .await
            .map(Into::into)
            .map_err(|e| self.read_error(e))
    }

    /// The stored EMA pair and the touch it is current as of, at this
    /// block: the pair from its storage word, the touch from `rates()`. The
    /// same `Emas` the `RatesAndEmasRefreshed` event carries, before any
    /// advance to a later timestamp; [`Self::mark`] is the advanced form.
    pub async fn emas(&self) -> Result<Emas> {
        let chain = &self.market.chain;
        let (rates, stored) = tokio::try_join!(self.rates(), async {
            chain
                .stored_emas_at(self.market.perp, self.id())
                .await
                .map_err(|e| pinned_read_error(e, self.block.number))
        })?;
        Ok(Emas {
            amm_price: Price::from_x96(U256::from(stored.amm)),
            index: Price::from_x96(U256::from(stored.index)),
            last_touch: rates.last_touch,
        })
    }

    /// The pool's current tick.
    pub async fn pool_tick(&self) -> Result<i32> {
        let perp = Perp::new(self.market.perp, self.market.chain.provider());
        let state = perp
            .poolState()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        Ok(i24_to_i32(state.tick))
    }

    /// The USDC the perp holds, which backs its positions' margin and also
    /// its insurance fund and uncollected fees.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] if the balance does not fit `i128`.
    pub async fn collateral(&self) -> Result<f64> {
        let chain = &self.market.chain;
        let raw: U256 = IERC20::new(chain.deployments().usdc, chain.provider())
            .balanceOf(self.market.perp)
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        Ok(usdc_from_atoms(raw, "collateral")?)
    }

    /// The market's taker capacity and the open interest drawing on it, in
    /// one multicall.
    ///
    /// [`MarketCapacity`] derives each side's headroom and utilization as
    /// the contract computes them.
    pub async fn capacity(&self) -> Result<MarketCapacity> {
        let perp = Perp::new(self.market.perp, self.market.chain.provider());
        let (capacity, oi) = self
            .market
            .chain
            .multicall_at(self.id())
            .add(perp.capacity())
            .add(perp.openInterest())
            .aggregate()
            .await
            .map_err(|e| self.multicall_read_error(e))?;
        Ok(MarketCapacity {
            block: self.block,
            capacity: capacity.into(),
            open_interest: oi.into(),
        })
    }

    /// The `IMarginRatios` module's maker and taker init / liquidation /
    /// backstop thresholds, as fractions.
    ///
    /// `modules()` and the two getters are read at this block, so a
    /// governance module swap or ratio update cannot straddle the result.
    /// Verified live 2026-09-07 on HORMUZ-TRAFFIC's module
    /// `0x8afca53c52b1f02d76aefb811c6b08f4bd3e4cf9` (Arbitrum One):
    /// `makerMarginRatios()` = (1000000, 900000, 800000), i.e.
    /// 1.0 / 0.9 / 0.8; `takerMarginRatios()` = (100000, 50000, 20000),
    /// i.e. 0.1 / 0.05 / 0.02.
    ///
    /// # Errors
    ///
    /// [`ContractError::ModuleNotRegistered`] when `modules().marginRatios`
    /// is the zero address.
    pub async fn margin_ratios(&self) -> Result<MarginRatios> {
        let provider = self.market.chain.provider();
        let perp = Perp::new(self.market.perp, provider);
        let modules = perp
            .modules()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let ratios = IMarginRatios::new(
            registered_module(modules.marginRatios, "IMarginRatios")?,
            provider,
        );
        let maker_call = ratios.makerMarginRatios().block(self.id());
        let taker_call = ratios.takerMarginRatios().block(self.id());
        let (maker, taker) = tokio::try_join!(maker_call.call(), taker_call.call())
            .map_err(|e| self.read_error(e))?;
        let triple = |init, liq, backstop| -> Result<MarginRatioTriple> {
            Ok(MarginRatioTriple {
                init: Ratio::from_e6(u24_to_u32(init))?,
                liquidation: Ratio::from_e6(u24_to_u32(liq))?,
                backstop: Ratio::from_e6(u24_to_u32(backstop))?,
            })
        };
        Ok(MarginRatios {
            maker: triple(maker.init, maker.liq, maker.backstop)?,
            taker: triple(taker.init, taker.liq, taker.backstop)?,
        })
    }

    /// What the contract marks from at this block: the pool price, the
    /// beacon index, and the stored EMAs advanced to the block timestamp.
    /// One multicall for the `Perp` views, then the beacon's `index()`,
    /// both pinned here.
    ///
    /// # Errors
    ///
    /// [`ContractError::ModuleNotRegistered`] when the perp has no beacon.
    pub async fn mark(&self) -> Result<Mark> {
        let views = self.perp_views().await?;
        self.mark_from(&views).await
    }

    /// The pool at this block, exact to the deployed Perp
    /// (`perpcity-contracts@4bbe554f`): its price and active liquidity,
    /// its initialized ticks from the PoolManager's bitmap, and the swap
    /// bounds the price-impact module sets at this block's [`Mark`]. What
    /// a taker swap is quoted against.
    ///
    /// # Errors
    ///
    /// [`ContractError::ModuleNotRegistered`] when the perp has no beacon
    /// or no price-impact module; [`ContractError::StorageReadFailed`]
    /// when the tick map does not reproduce the pool's active liquidity.
    pub async fn pool(&self) -> Result<PoolSnapshot> {
        let immutables = self.market.immutables().await?;
        let views = self.perp_views().await?;
        let mark = self.mark_from(&views).await?;
        let bounds = self.impact_bounds(views.modules.priceImpact, &mark).await?;
        let ticks = self.tick_map(&immutables).await?;

        let pool = &views.pool_state;
        let tick = i24_to_i32(pool.tick);
        let reconstructed = active_liquidity(&ticks, tick)?;
        if reconstructed != LUnits::new(pool.liquidity) {
            return Err(ContractError::StorageReadFailed {
                context: format!(
                    "tick map liquidity mismatch: reconstructed {}, pool {}",
                    reconstructed.units(),
                    pool.liquidity
                ),
                source: None,
            }
            .into());
        }
        Ok(PoolSnapshot {
            block: self.block,
            sqrt_price: SqrtPrice::from_x96(pool.sqrtPrice.to::<U256>()),
            tick,
            liquidity: LUnits::new(pool.liquidity),
            ticks,
            protocol_sqrt_min: SqrtPrice::from_x96(MIN_SWAP_SQRT_PRICE_X96),
            protocol_sqrt_max: SqrtPrice::from_x96(MAX_SWAP_SQRT_PRICE_X96),
            impact_sqrt_min: SqrtPrice::from_x96(bounds.sqrtMin),
            impact_sqrt_max: SqrtPrice::from_x96(bounds.sqrtMax),
        })
    }

    /// The `Perp` views a mark and the pool are built from, in one
    /// multicall at this block.
    async fn perp_views(&self) -> Result<PerpViews> {
        let perp = Perp::new(self.market.perp, self.market.chain.provider());
        let (modules, pool_state, rates, ema_window) = self
            .market
            .chain
            .multicall_at(self.id())
            .add(perp.modules())
            .add(perp.poolState())
            .add(perp.rates())
            .add(perp.EMA_WINDOW())
            .aggregate()
            .await
            .map_err(|e| self.multicall_read_error(e))?;
        Ok(PerpViews {
            modules,
            pool_state,
            last_touch: rates.lastTouch.to::<u64>(),
            ema_window: ema_window_secs(ema_window)?,
        })
    }

    /// The mark from the views: the beacon's `index()` and the stored EMAs'
    /// storage word at this block, read together, then the pair advanced
    /// to it.
    pub(super) async fn mark_from(&self, views: &PerpViews) -> Result<Mark> {
        let beacon = registered_module(views.modules.beacon, "IBeacon")?;
        let chain = &self.market.chain;
        let (index, stored_emas) = tokio::try_join!(
            chain.index_x96_at(beacon, self.id()),
            chain.stored_emas_at(self.market.perp, self.id()),
        )
        .map_err(|e| pinned_read_error(e, self.block.number))?;
        Ok(Mark::advanced(
            self.block,
            Price::from_x96(views.pool_state.ammPrice),
            Price::from_x96(index),
            stored_emas,
            views.last_touch,
            views.ema_window,
        )?)
    }

    /// The price-impact module's swap bounds at this mark.
    async fn impact_bounds(
        &self,
        price_impact: Address,
        mark: &Mark,
    ) -> Result<IPriceImpact::sqrtPriceBoundsReturn> {
        let module = registered_module(price_impact, "IPriceImpact")?;
        IPriceImpact::new(module, self.market.chain.provider())
            .sqrtPriceBounds(
                mark.pool_price.x96(),
                mark.index.x96(),
                U256::from(mark.emas.amm),
                U256::from(mark.emas.index),
            )
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))
    }

    /// The pool's initialized ticks at this block: the PoolManager's tick
    /// bitmap over the whole tick range, then the tick word of every set
    /// bit, both by pinned `extsload`.
    async fn tick_map(
        &self,
        immutables: &MarketImmutables,
    ) -> Result<BTreeMap<i32, TickLiquidity>> {
        let MarketImmutables {
            pool_id,
            tick_spacing: spacing,
            ..
        } = *immutables;
        let chain = &self.market.chain;
        let manager = IPoolManagerState::new(chain.deployments().pool_manager, chain.provider());

        let min_word = MIN_TICK.div_euclid(spacing).div_euclid(256);
        let max_word = MAX_TICK.div_euclid(spacing).div_euclid(256);
        let bitmap_slots: Vec<B256> = (min_word..=max_word)
            .map(|word| B256::from(v4_tick_bitmap_slot(pool_id, word)))
            .collect();
        let bitmaps = manager
            .extsload_1(bitmap_slots)
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let initialized: Vec<i32> = bitmaps
            .into_iter()
            .enumerate()
            .flat_map(|(offset, bitmap)| {
                let word = min_word + offset as i32;
                let bits = U256::from_be_bytes(bitmap.0);
                (0..256i32)
                    .filter(move |&bit| bits.bit(bit as usize))
                    .map(move |bit| (word * 256 + bit) * spacing)
            })
            .filter(|tick| (MIN_TICK..=MAX_TICK).contains(tick))
            .collect();
        if initialized.is_empty() {
            return Ok(BTreeMap::new());
        }

        let tick_slots: Vec<B256> = initialized
            .iter()
            .map(|&tick| B256::from(v4_tick_slot(pool_id, tick)))
            .collect();
        let words = manager
            .extsload_1(tick_slots)
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        Ok(initialized
            .into_iter()
            .zip(words)
            .map(|(tick, word)| {
                // `liquidityNet` in the high half, `liquidityGross` in the low.
                let raw = U256::from_be_bytes(word.0);
                let gross = LUnits::new((raw & U256::from(u128::MAX)).to::<u128>());
                let net = LDelta::new((raw >> 128usize).to::<u128>() as i128);
                (tick, TickLiquidity { gross, net })
            })
            .collect())
    }

    /// A multicall's failure, classified as [`multicall_error`] does and
    /// then typed for this handle's block by [`pinned_read_error`].
    pub(super) fn multicall_read_error(&self, error: MulticallError) -> PerpCityError {
        pinned_read_error(multicall_error(error), self.block.number)
    }
}

/// One `aggregate3` row decoded as `C`'s return, or why not: a reverted
/// row is [`ContractError::MulticallFailed`], one that does not decode is
/// [`ValidationError::DecodeFailed`]; neither is transient.
pub(super) fn decode_row<C: SolCall>(
    pos_id: U256,
    row: &IMulticall3::Result,
    what: &str,
) -> Result<C::Return> {
    if !row.success {
        return Err(ContractError::MulticallFailed {
            reason: format!("{what} {pos_id} row read reverted"),
        }
        .into());
    }
    C::abi_decode_returns(&row.returnData).map_err(|e| {
        ValidationError::DecodeFailed {
            context: format!("{what} {pos_id}: {e}"),
        }
        .into()
    })
}

/// A pinned read's failure, with the two answers a node gives about the
/// block itself typed for block `number`: its refusal to serve pruned
/// state is [`ContractError::StateUnavailable`], which is not transient,
/// and its not having the block at all is
/// [`ContractError::BlockUnavailable`], which is — a replica behind the
/// one that named the block will catch up. Every other failure passes
/// through.
pub(super) fn pinned_read_error(error: PerpCityError, number: u64) -> PerpCityError {
    let PerpCityError::Abi(alloy::contract::Error::TransportError(RpcError::ErrorResp(payload))) =
        &error
    else {
        return error;
    };
    let message = payload.message.to_ascii_lowercase();
    if state_pruned(&message) {
        ContractError::StateUnavailable { number }.into()
    } else if block_missing(&message) {
        ContractError::BlockUnavailable { number }.into()
    } else {
        error
    }
}

/// The `Perp` views a mark, the pool and the maker-equity batch are built
/// from, read together at one block.
pub(super) struct PerpViews {
    /// `modules()`.
    pub(super) modules: Modules,
    /// `poolState()`.
    pub(super) pool_state: Perp::poolStateReturn,
    /// `rates().lastTouch`.
    pub(super) last_touch: u64,
    /// `EMA_WINDOW()`, in seconds.
    pub(super) ema_window: u64,
}

/// The contract's `EMA_WINDOW` (seconds) narrowed to the width
/// [`Mark::advanced`] takes.
pub(super) fn ema_window_secs(ema_window: U256) -> Result<u64> {
    u64::try_from(ema_window).map_err(|_| {
        ValidationError::Overflow {
            context: "EMA window".into(),
        }
        .into()
    })
}

/// Whether a node's (lower-cased) error message is its refusal to serve
/// pruned state: Nitro's "historical state … is not available", geth's
/// "missing trie node". The message is the only place a node says so.
fn state_pruned(message: &str) -> bool {
    message.contains("historical state") || message.contains("missing trie node")
}

/// Whether a node's (lower-cased) error message says it does not have the
/// block a call was pinned to: geth's "header not found" for a hash or
/// number it has not seen, and the "unknown block" / "block not found"
/// wordings of other clients and gateways.
fn block_missing(message: &str) -> bool {
    message.contains("header not found")
        || message.contains("unknown block")
        || message.contains("block not found")
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, I256, Uint};
    use alloy::transports::TransportErrorKind;

    use super::*;
    use crate::client::mock::{self, PERP, Rpc, e6, returns, x96};
    use crate::constants::SNAPSHOT_BLOCK_LAG;
    use crate::contracts::{IBeacon, Modules, Rates};
    use crate::math::capacity::Capacity;
    use crate::math::pricing::PricePair;
    use crate::units::{PerSide, PerpAtoms};

    const TIMESTAMP: u64 = 1_700_000_000;
    /// The market's tick spacing in these tests.
    const SPACING: i32 = 30;

    /// The deployed HORMUZ-TRAFFIC module values (2026-09-07): maker
    /// 1.0 / 0.9 / 0.8, taker 0.1 / 0.05 / 0.02. The ratios are stored as
    /// the module's own `uint24`, so the fractions are derived and nothing
    /// round-trips through a float.
    #[test]
    fn margin_ratio_triple_holds_the_modules_own_integers() {
        let triple = |init, liquidation, backstop| MarginRatioTriple {
            init: Ratio::from_e6(init).unwrap(),
            liquidation: Ratio::from_e6(liquidation).unwrap(),
            backstop: Ratio::from_e6(backstop).unwrap(),
        };
        let maker = triple(1_000_000, 900_000, 800_000);
        assert_eq!(
            (
                maker.init.fraction(),
                maker.liquidation.fraction(),
                maker.backstop.fraction()
            ),
            (1.0, 0.9, 0.8)
        );
        let taker = triple(100_000, 50_000, 20_000);
        assert_eq!(
            (
                taker.init.fraction(),
                taker.liquidation.fraction(),
                taker.backstop.fraction()
            ),
            (0.1, 0.05, 0.02)
        );
        for e6 in [0u32, 1, 3, 333_333, 999_999, (1 << 24) - 1] {
            assert_eq!(triple(e6, e6, e6).init.e6(), e6);
        }

        let ratios = MarginRatios { maker, taker };
        let json = serde_json::to_string(&ratios).unwrap();
        let recovered: MarginRatios = serde_json::from_str(&json).unwrap();
        assert_eq!(ratios, recovered);
    }

    /// A handle pinned to block 92 by name, over a mocked reader.
    async fn state() -> (StateAt, Rpc) {
        let (client, rpc) = mock::client();
        rpc.block(92, TIMESTAMP);
        let state = client.market().state_at(92).await.unwrap();
        (state, rpc)
    }

    #[tokio::test]
    async fn state_pins_the_lagged_snapshot_block() {
        let (client, rpc) = mock::client();
        rpc.quantity(100);
        let hash = rpc.block(100 - SNAPSHOT_BLOCK_LAG, TIMESTAMP);

        let state = client.market().state().await.unwrap();
        assert_eq!(
            state.block(),
            BlockContext {
                number: 100 - SNAPSHOT_BLOCK_LAG,
                hash,
                timestamp: TIMESTAMP,
            }
        );
        assert_eq!(state.market().perp(), PERP);
        assert!(rpc.is_drained(), "blockNumber and the header, nothing else");
    }

    #[tokio::test]
    async fn state_at_pins_the_named_block_without_asking_for_the_head() {
        let (state, rpc) = state().await;
        assert_eq!(state.block().number, 92);
        assert!(rpc.is_drained(), "the header alone");
    }

    #[tokio::test]
    async fn a_missing_header_is_a_failed_read_not_the_head() {
        let (client, rpc) = mock::client();
        rpc.no_block();
        let Err(PerpCityError::Contract(ContractError::BlockUnavailable { number })) =
            client.market().state_at(7).await
        else {
            panic!("an absent header must fail the handle");
        };
        assert_eq!(number, 7);
    }

    /// A full node keeps the header and prunes the state: the handle is
    /// handed out and the read names the condition, which no retry fixes.
    #[tokio::test]
    async fn pruned_state_is_a_typed_permanent_failure() {
        for message in [
            "historical state 1682db2b34068813264a4d89e2d8d02b0703b6f0e6acd8437d8080d98a5db56b is not available",
            "missing trie node 0x1682db2b (path ) state 0x1682db2b is not available",
        ] {
            let (state, rpc) = state().await;
            rpc.fails(message);
            let error = state.solvency().await.unwrap_err();
            let PerpCityError::Contract(ContractError::StateUnavailable { number }) = error else {
                panic!("{message:?} must type as StateUnavailable, got {error}");
            };
            assert_eq!(number, 92);
            assert!(
                !error.is_transient(),
                "an archive endpoint is the fix, not a retry"
            );
        }
    }

    /// Any other node failure keeps its own shape.
    #[tokio::test]
    async fn other_read_failures_pass_through() {
        let (state, rpc) = state().await;
        rpc.fails("execution aborted (timeout = 5s)");
        assert!(matches!(
            state.pool_tick().await.unwrap_err(),
            PerpCityError::Abi(_)
        ));
    }

    #[tokio::test]
    async fn solvency_is_the_contracts_atoms() {
        let (state, rpc) = state().await;
        rpc.call::<Perp::solvencyStateCall>(&mock::solvency(1_500_000, 250_000_000));
        assert_eq!(
            state.solvency().await.unwrap(),
            SolvencyState {
                bad_debt: UsdcAtoms::new(1_500_000),
                total_margin: UsdcAtoms::new(250_000_000),
            }
        );
    }

    #[tokio::test]
    async fn next_pos_id_is_a_count_and_refuses_a_broken_one() {
        let (state, rpc) = state().await;
        rpc.call::<Perp::nextPosIdCall>(&U256::from(1_717));
        assert_eq!(state.next_pos_id().await.unwrap(), 1_717);

        rpc.call::<Perp::nextPosIdCall>(&U256::from(u64::MAX).saturating_add(U256::from(1)));
        let Err(PerpCityError::Validation(ValidationError::Overflow { .. })) =
            state.next_pos_id().await
        else {
            panic!("a count past u64 is a broken read, not a saturated one");
        };
    }

    /// `None` for the empty struct the contract returns for closed and
    /// never-minted ids, where `get_position` fails — the two callers
    /// want different things from the same bytes.
    #[tokio::test]
    async fn an_empty_position_is_none_here_and_not_found_there() {
        let (state, rpc) = state().await;
        rpc.call::<Perp::positionsCall>(&mock::position(0));
        assert!(state.position(U256::from(3)).await.unwrap().is_none());

        rpc.call::<Perp::positionsCall>(&mock::position(5_000_000));
        let position = state.position(U256::from(4)).await.unwrap().unwrap();
        assert_eq!(position.margin, 5_000_000);

        rpc.call::<Perp::positionsCall>(&mock::position(0));
        let Err(PerpCityError::Contract(ContractError::PositionNotFound { pos_id })) =
            state.market().get_position(U256::from(3)).await
        else {
            panic!("get_position keeps its error");
        };
        assert_eq!(pos_id, U256::from(3));
    }

    /// A position with exposure but no margin is still a position: the
    /// contract's empty struct has both zero.
    #[tokio::test]
    async fn a_position_with_exposure_and_no_margin_is_present() {
        let (state, rpc) = state().await;
        let mut exposed = mock::position(0);
        exposed.delta = I256::try_from(-7).unwrap();
        rpc.call::<Perp::positionsCall>(&exposed);
        assert!(state.position(U256::from(9)).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn maker_band_is_none_without_liquidity() {
        let (state, rpc) = state().await;
        rpc.call::<Perp::makerDetailsCall>(&mock::maker(-60, 60, 0));
        assert_eq!(state.maker_band(U256::from(1)).await.unwrap(), None);

        rpc.call::<Perp::makerDetailsCall>(&mock::maker(38_340, 38_430, 97_506_535));
        assert_eq!(
            state.maker_band(U256::from(1_691)).await.unwrap(),
            Some(MakerBand::new(
                TickRange::new(38_340, 38_430).unwrap(),
                LUnits::new(97_506_535)
            ))
        );
    }

    /// A range the contract could never store still cannot reach the
    /// math: it fails here, as the chain boundary.
    #[tokio::test]
    async fn a_malformed_range_fails_at_the_read() {
        let (state, rpc) = state().await;
        rpc.call::<Perp::makerDetailsCall>(&mock::maker(60, -60, 5));
        assert!(matches!(
            state.maker_band(U256::from(2)).await,
            Err(PerpCityError::Validation(
                ValidationError::InvalidTickRange { .. }
            ))
        ));
    }

    #[tokio::test]
    async fn pool_tick_is_the_slot_tick() {
        let (state, rpc) = state().await;
        rpc.call::<Perp::poolStateCall>(&mock::pool_state_at_tick(38_340));
        assert_eq!(state.pool_tick().await.unwrap(), 38_340);
    }

    #[tokio::test]
    async fn collateral_is_the_perps_usdc_balance() {
        let (state, rpc) = state().await;
        rpc.call::<IERC20::balanceOfCall>(&U256::from(3_265_080_000u64));
        assert_eq!(state.collateral().await.unwrap(), 3_265.08);
        assert!(rpc.is_drained());
    }

    // ── One handle, one block ─────────────────────────────────────────

    /// The `IMarginRatios` module's two triples, as the mock serves them.
    fn margin_ratio_answers(rpc: &Rpc) {
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
    }

    /// The point of the handle: several reads resolve the header once
    /// and every result is from that block.
    #[tokio::test]
    async fn one_handle_resolves_the_header_once_for_every_read() {
        let (state, rpc) = state().await;
        rpc.aggregate(
            92,
            [
                returns::<Perp::capacityCall>(&mock::capacity(10, 20)),
                returns::<Perp::openInterestCall>(&mock::open_interest(3, 4)),
            ],
        );
        margin_ratio_answers(&rpc);

        let capacity = state.capacity().await.unwrap();
        let ratios = state.margin_ratios().await.unwrap();
        assert_eq!(capacity.block, state.block());
        assert_eq!(capacity.block.number, 92);
        assert_eq!(ratios.maker.init, Ratio::ONE);
        assert!(
            rpc.is_drained(),
            "the header once at construction, then one multicall and three calls"
        );
    }

    /// The mark is the views multicall, then the beacon's index and the
    /// EMA slot together, all at the handle's block; with nothing to
    /// advance, the EMAs are the stored pair and the fair price of equal
    /// inputs is that price.
    #[tokio::test]
    async fn the_mark_is_one_multicall_and_the_index_at_the_handles_block() {
        let one = x96(1, 0);
        let (state, rpc) = state().await;
        perp_views_answers(&rpc, 92, 0);
        rpc.call::<IBeacon::indexCall>(&one);
        rpc.storage(mock::emas_word(one.to::<u128>(), one.to::<u128>()));

        let mark = state.mark().await.unwrap();
        assert_eq!(mark.block, state.block());
        assert_eq!(
            (mark.pool_price, mark.index),
            (Price::from_x96(one), Price::from_x96(one))
        );
        assert_eq!(
            mark.emas,
            PricePair {
                amm: one.to::<u128>(),
                index: one.to::<u128>()
            }
        );
        assert_eq!(mark.fair_price(), Price::from_x96(one));
        assert!(rpc.is_drained(), "one multicall, the index and the slot");
    }

    /// A replica that has the header but not the block a read is pinned
    /// to — the one that named the block is ahead of it — is a transient
    /// failure that names the block, the same as a missing header at
    /// construction.
    #[tokio::test]
    async fn a_replica_without_the_block_is_block_unavailable_and_transient() {
        let (state, rpc) = state().await;
        rpc.fails("header not found");

        let error = state.solvency().await.unwrap_err();
        let PerpCityError::Contract(ContractError::BlockUnavailable { number }) = error else {
            panic!(
                "a pinned read at a block the replica lacks must type as BlockUnavailable, got {error}"
            );
        };
        assert_eq!(number, 92);
        assert!(error.is_transient());
    }

    /// A pruned block fails a multicall read the same way it fails a
    /// single call: typed, and not transient.
    #[tokio::test]
    async fn pruned_state_fails_a_multicall_read_the_same_way() {
        let (state, rpc) = state().await;
        rpc.fails("historical state 1682db2b is not available");

        let error = state.capacity().await.unwrap_err();
        let PerpCityError::Contract(ContractError::StateUnavailable { number }) = error else {
            panic!("a multicall over pruned state must type as StateUnavailable, got {error}");
        };
        assert_eq!(number, 92);
        assert!(!error.is_transient());
    }

    // ── The batched row read ──────────────────────────────────────────

    /// Every id gets its row from one multicall, in input order: a present
    /// position, an empty row, and a reverted row that fails alone and
    /// deterministically.
    #[tokio::test]
    async fn positions_read_every_row_in_one_multicall() {
        let (state, rpc) = state().await;
        let pos_ids = [U256::from(11u8), U256::from(22u8), U256::from(33u8)];
        rpc.aggregate3(vec![
            mock::ok_row(returns::<Perp::positionsCall>(&mock::position(5_000_000))),
            mock::ok_row(returns::<Perp::positionsCall>(&mock::position(0))),
            mock::failed_row(),
        ]);

        let rows = state.positions(&pos_ids).await;
        assert_eq!(rows.iter().map(|r| r.pos_id).collect::<Vec<_>>(), pos_ids);
        assert_eq!(
            rows[0].row.as_ref().unwrap().as_ref().unwrap().margin,
            5_000_000
        );
        assert!(rows[1].row.as_ref().unwrap().is_none());
        let Err(err) = &rows[2].row else {
            panic!("pos 33's row reverted");
        };
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::MulticallFailed { .. })
            ),
            "{err}"
        );
        assert!(!err.is_transient(), "a reverted row reverts again");
        assert!(rpc.is_drained(), "one multicall");
    }

    /// A multicall the replica cannot serve fails every id of the chunk
    /// with the typed, transient answer; no id is dropped.
    #[tokio::test]
    async fn a_failed_row_multicall_fails_every_id_of_the_chunk_naming_the_block() {
        let (state, rpc) = state().await;
        rpc.fails("header not found");

        let rows = state.positions(&[U256::ONE, U256::from(2u8)]).await;
        assert_eq!(rows.len(), 2);
        for row in &rows {
            let Err(err) = &row.row else {
                panic!("every id of the chunk fails");
            };
            assert!(
                matches!(
                    err,
                    PerpCityError::Contract(ContractError::BlockUnavailable { number: 92 })
                ),
                "{err}"
            );
            assert!(err.is_transient());
        }
        assert!(rpc.is_drained());
    }

    /// A multicall answering the wrong number of rows fails the chunk
    /// rather than misaligning ids and rows.
    #[tokio::test]
    async fn a_short_multicall_answer_fails_the_chunk() {
        let (state, rpc) = state().await;
        rpc.aggregate3(vec![mock::ok_row(returns::<Perp::positionsCall>(
            &mock::position(1),
        ))]);

        let rows = state.positions(&[U256::ONE, U256::from(2u8)]).await;
        for row in &rows {
            let Err(err) = &row.row else {
                panic!("a short answer fails the chunk");
            };
            assert!(
                matches!(
                    err,
                    PerpCityError::Contract(ContractError::StorageReadFailed { source: None, .. })
                ),
                "{err}"
            );
            assert!(!err.is_transient());
        }
    }

    #[tokio::test]
    async fn no_ids_is_no_request() {
        let (state, rpc) = state().await;
        assert!(state.positions(&[]).await.is_empty());
        assert!(rpc.is_drained());
    }

    /// A chunk-wide failure fans out to every id keeping what a retry loop
    /// needs: a transport cause stays transient, a block the replica lacks
    /// stays `BlockUnavailable`, pruned state stays `StateUnavailable`, and
    /// a deterministic failure stays non-transient.
    #[test]
    fn a_chunk_failure_keeps_its_classification_on_every_id() {
        let transport = ChunkReadFailure::new(
            "row multicall",
            PerpCityError::Abi(alloy::contract::Error::TransportError(
                TransportErrorKind::custom_str("replica timed out"),
            )),
        );
        let (first, second) = (transport.error(), transport.error());
        assert!(first.is_transient() && second.is_transient(), "{first}");
        assert!(first.to_string().contains("row multicall"));

        let behind = ChunkReadFailure::new(
            "row multicall",
            ContractError::BlockUnavailable { number: 92 }.into(),
        );
        assert!(matches!(
            behind.error(),
            PerpCityError::Contract(ContractError::BlockUnavailable { number: 92 })
        ));
        assert!(behind.error().is_transient());

        let pruned = ChunkReadFailure::new(
            "row multicall",
            ContractError::StateUnavailable { number: 92 }.into(),
        );
        assert!(matches!(
            pruned.error(),
            PerpCityError::Contract(ContractError::StateUnavailable { number: 92 })
        ));
        assert!(!pruned.error().is_transient());

        let deterministic = ChunkReadFailure::new(
            "row multicall",
            ContractError::MulticallFailed {
                reason: "3 results, expected 4".into(),
            }
            .into(),
        );
        let err = deterministic.error();
        assert!(!err.is_transient(), "{err}");
        assert!(err.to_string().contains("3 results, expected 4"));
    }

    // ── The single-read conveniences: a fresh lagged handle each ──────

    /// Both triples come back as fractions. The two reads share one ABI
    /// shape, so distinct values are what pin their order.
    #[tokio::test]
    async fn margin_ratios_decode_both_triples_as_fractions() {
        let (client, rpc) = mock::client();
        rpc.quantity(100);
        rpc.block(100 - SNAPSHOT_BLOCK_LAG, TIMESTAMP);
        margin_ratio_answers(&rpc);

        let ratios = client.market().get_margin_ratios().await.unwrap();
        assert_eq!(
            (
                ratios.maker.init.e6(),
                ratios.maker.liquidation.e6(),
                ratios.maker.backstop.e6()
            ),
            (1_000_000, 900_000, 800_000)
        );
        assert_eq!(
            (
                ratios.taker.init.e6(),
                ratios.taker.liquidation.e6(),
                ratios.taker.backstop.e6()
            ),
            (100_000, 50_000, 20_000)
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

        let err = client.market().get_margin_ratios().await.unwrap_err();
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
        rpc.block(100 - SNAPSHOT_BLOCK_LAG, TIMESTAMP);
        rpc.call::<Perp::modulesCall>(&Modules {
            marginRatios: Address::ZERO,
            ..mock::modules()
        });

        let err = client.market().get_margin_ratios().await.unwrap_err();
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

    /// Capacity and open interest are read in one batch at the lagged
    /// block, and the result carries that block.
    #[tokio::test]
    async fn capacity_carries_the_lagged_block_it_was_read_at() {
        let (client, rpc) = mock::client();
        rpc.quantity(100);
        let hash = rpc.block(100 - SNAPSHOT_BLOCK_LAG, TIMESTAMP);
        rpc.aggregate(
            100 - SNAPSHOT_BLOCK_LAG,
            [
                returns::<Perp::capacityCall>(&mock::capacity(10, 20)),
                returns::<Perp::openInterestCall>(&mock::open_interest(3, 4)),
            ],
        );

        assert_eq!(
            client.market().get_capacity().await.unwrap(),
            MarketCapacity {
                block: BlockContext {
                    number: 100 - SNAPSHOT_BLOCK_LAG,
                    hash,
                    timestamp: TIMESTAMP,
                },
                capacity: Capacity {
                    long: PerpAtoms::new(10),
                    short: PerpAtoms::new(20),
                },
                open_interest: PerSide::new(PerpAtoms::new(3), PerpAtoms::new(4)),
            }
        );
        assert!(rpc.is_drained(), "blockNumber, block, one multicall");
    }

    // ── The pool: immutables, pinned views, and the tick map ──────────

    /// The pool's id, as `POOL_ID()` reports it.
    const POOL_ID: B256 = B256::repeat_byte(0x99);
    /// When the market was last touched, and the block time in these
    /// tests, so the stored EMAs need no advancing.
    const TOUCHED_AT: u64 = TIMESTAMP;
    /// Bitmap words the tick map covers at this spacing:
    /// `MIN_TICK.div_euclid(30) = -4606`, `.div_euclid(256) = -18`, up to
    /// `4606.div_euclid(256) = 17` — 36 words, word `w` at offset `w + 18`.
    const BITMAP_WORDS: usize = 36;

    /// The two immutables the first snapshot reads, in `try_join!` order.
    fn immutables_answers(rpc: &Rpc) {
        rpc.call::<Perp::POOL_IDCall>(&POOL_ID);
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key(SPACING));
    }

    /// The `Perp` views behind a mark, in one multicall at `block`: pool
    /// price 1.0, last touched at [`TOUCHED_AT`].
    fn perp_views_answers(rpc: &Rpc, block: u64, liquidity: u128) {
        let one = x96(1, 0);
        rpc.aggregate(
            block,
            [
                returns::<Perp::modulesCall>(&mock::modules()),
                returns::<Perp::poolStateCall>(&Perp::poolStateReturn {
                    sqrtPrice: Uint::from(1u8) << 96,
                    liquidity,
                    ..mock::pool_state(one)
                }),
                returns::<Perp::ratesCall>(&Rates {
                    lastTouch: Uint::from(TOUCHED_AT),
                    ..mock::rates(0)
                }),
                returns::<Perp::EMA_WINDOWCall>(&U256::from(3_600u32)),
            ],
        );
    }

    /// Everything after the immutables: the lagged block, the views
    /// multicall, the beacon, the impact bounds, then the bitmap and — if
    /// any tick is set — the tick words. Returns the pinned block's hash.
    fn snapshot_answers(
        rpc: &Rpc,
        liquidity: u128,
        bitmap: Vec<B256>,
        tick_words: Option<Vec<B256>>,
    ) -> B256 {
        let one = x96(1, 0);
        rpc.quantity(100);
        let hash = rpc.block(100 - SNAPSHOT_BLOCK_LAG, TOUCHED_AT);
        perp_views_answers(rpc, 100 - SNAPSHOT_BLOCK_LAG, liquidity);
        rpc.call::<IBeacon::indexCall>(&one);
        rpc.storage(mock::emas_word(one.to::<u128>(), one.to::<u128>()));
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

    /// Liquidity standing at a tick, gross then net.
    fn tick_liquidity(gross: u128, net: i128) -> TickLiquidity {
        TickLiquidity {
            gross: LUnits::new(gross),
            net: LDelta::new(net),
        }
    }

    fn expected_snapshot(hash: B256, liquidity: LUnits) -> PoolSnapshot {
        let one = x96(1, 0);
        PoolSnapshot {
            block: BlockContext {
                number: 100 - SNAPSHOT_BLOCK_LAG,
                hash,
                timestamp: TOUCHED_AT,
            },
            sqrt_price: SqrtPrice::from_x96(one),
            tick: 0,
            liquidity,
            ticks: BTreeMap::new(),
            protocol_sqrt_min: SqrtPrice::from_x96(MIN_SWAP_SQRT_PRICE_X96),
            protocol_sqrt_max: SqrtPrice::from_x96(MAX_SWAP_SQRT_PRICE_X96),
            impact_sqrt_min: SqrtPrice::from_x96(one >> 1),
            impact_sqrt_max: SqrtPrice::from_x96(one << 1),
        }
    }

    /// An empty bitmap means no tick words are asked for, and the empty
    /// tick map reconciles with a pool holding no liquidity.
    #[tokio::test]
    async fn an_empty_pool_skips_the_tick_words() {
        let (client, rpc) = mock::client();
        immutables_answers(&rpc);
        let hash = snapshot_answers(&rpc, 0, bitmap(&[]), None);

        assert_eq!(
            client.market().get_pool_snapshot().await.unwrap(),
            expected_snapshot(hash, LUnits::ZERO)
        );
        assert!(rpc.is_drained(), "eight answers, none left over");
    }

    /// Set bits become ticks at `compressed * spacing`, their words are
    /// read in bitmap order, and the net liquidity of every tick at or
    /// below the pool's must add up to what the pool reports.
    #[tokio::test]
    async fn the_pool_rebuilds_its_tick_map_from_the_bitmap() {
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

        let snapshot = client.market().get_pool_snapshot().await.unwrap();
        assert_eq!(
            snapshot,
            PoolSnapshot {
                ticks: BTreeMap::from([
                    (-60, tick_liquidity(L, L as i128)),
                    (60, tick_liquidity(L, -(L as i128))),
                ]),
                ..expected_snapshot(hash, LUnits::new(L))
            }
        );
        assert!(rpc.is_drained(), "nine answers");
    }

    /// A tick map whose ticks do not add up to the pool's active liquidity
    /// is a wrong read, not a snapshot.
    #[tokio::test]
    async fn a_tick_map_that_does_not_reconcile_with_the_pool_is_rejected() {
        const L: u128 = 1_000;
        let (client, rpc) = mock::client();
        immutables_answers(&rpc);
        snapshot_answers(
            &rpc,
            L + 1,
            bitmap(&[(17, 254), (18, 2)]),
            Some(vec![tick_word(L, L as i128), tick_word(L, -(L as i128))]),
        );

        let err = client.market().get_pool_snapshot().await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::StorageReadFailed { ref context, source: None })
                    if context.contains("liquidity mismatch")
            ),
            "{err}"
        );
        assert!(
            !err.is_transient(),
            "at a pinned hash a mismatch is the layout, not the replica"
        );
    }

    /// The immutables are read once per market: a second snapshot starts
    /// at the lagged block.
    #[tokio::test]
    async fn immutables_are_read_once_per_market() {
        let (client, rpc) = mock::client();
        immutables_answers(&rpc);
        snapshot_answers(&rpc, 0, bitmap(&[]), None);
        client.market().get_pool_snapshot().await.unwrap();

        let hash = snapshot_answers(&rpc, 0, bitmap(&[]), None);
        assert_eq!(
            client.market().get_pool_snapshot().await.unwrap(),
            expected_snapshot(hash, LUnits::ZERO)
        );
        assert!(rpc.is_drained(), "six answers: no immutables");
    }

    /// The immutables belong to the market, not the reader: a second
    /// reader of the same market over one chain reader inherits them, and
    /// a reader of another market reads its own.
    #[tokio::test]
    async fn immutables_are_shared_per_market_across_readers() {
        let (chain, rpc) = mock::chain();
        let (first, second) = (chain.market(PERP), chain.market(PERP));
        immutables_answers(&rpc);
        snapshot_answers(&rpc, 0, bitmap(&[]), None);
        first.get_pool_snapshot().await.unwrap();

        snapshot_answers(&rpc, 0, bitmap(&[]), None);
        second.get_pool_snapshot().await.unwrap();
        assert!(rpc.is_drained(), "the second reader skipped the immutables");

        let other = chain.market(Address::repeat_byte(0x12));
        immutables_answers(&rpc);
        snapshot_answers(&rpc, 0, bitmap(&[]), None);
        other.get_pool_snapshot().await.unwrap();
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

            let err = client.market().get_pool_snapshot().await.unwrap_err();
            assert!(
                matches!(
                    err,
                    PerpCityError::Validation(ValidationError::InvalidConfig { .. })
                ),
                "{err}"
            );
            assert!(rpc.is_drained(), "two answers each time");
        }
    }
}
