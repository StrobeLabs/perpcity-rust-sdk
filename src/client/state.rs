//! Reads of the market at one block, [`StateAt`].
//!
//! The counterpart of [`queries`](super::queries): there, every read is
//! independently current; here, a handle resolves one block and every read
//! on it is pinned to that block's hash. Two values that must agree — open
//! interest against capacity, a position against the mark — are read
//! through one handle, and agree by construction.

mod preview;

pub use preview::TvlCap;

use std::collections::BTreeMap;
use std::sync::Arc;

use alloy::eips::BlockId;
use alloy::primitives::{Address, B256, Bytes, Signed, U256, Uint};
use alloy::providers::{MulticallError, Provider};
use alloy::sol_types::{SolCall, SolError, SolType};
use alloy::transports::{RpcError, TransportError};
use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};

use crate::constants::{MAX_TICK, MIN_TICK, MULTICALL3};
use crate::contracts::{
    IERC20, IMarginRatios, IMulticall3, IPoolManagerState, IPriceImpact, Modules, Perp, Position,
    v1,
};
use crate::convert::unpack_balance_delta;
use crate::errors::{ContractError, PerpCityError, Result, TransactionError, ValidationError};
use crate::events::{CumulativesInfo, V1Cumulatives};
use crate::math::BlockContext;
use crate::math::capacity::{Capacity, MarketCapacity};
use crate::math::pricing::{BeaconIndex, Emas, Mark, PricePair, calculate_emas};
use crate::math::range::{MakerBand, TickRange};
use crate::math::swap::{PoolSnapshot, TakerFeeRates, TickLiquidity, active_liquidity};
use crate::storage::{v4_tick_bitmap_slot, v4_tick_slot};
use crate::units::{
    Funding, FundingRate, LDelta, LUnits, PerSide, PerpAtoms, PerpDelta, Price, Ratio, SqrtPrice,
    UsdcAtoms, UsdcDelta, UtilizationRate,
};

use super::market::MarketReader;
use super::queries::{Era, MarketImmutables, multicall_error, registered_module};
use super::transactions::{classify_simulation_failure, preflight_request};
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

/// The solvency books as one pinned read returned them, with the market and
/// the block they were read at. Made by [`StateAt::books`], so the books,
/// their market and their block cannot disagree; what [`Replay::read_books`](crate::history::Replay::read_books)
/// stands a fold on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Books {
    perp: Address,
    block: BlockContext,
    solvency: SolvencyState,
}

impl Books {
    /// Books a caller asserts were read from `perp` at `block`, for tests
    /// that have no node to read them from.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn stated(perp: Address, block: BlockContext, solvency: SolvencyState) -> Self {
        Self {
            perp,
            block,
            solvency,
        }
    }

    /// The market the books were read from.
    pub(crate) fn perp(&self) -> Address {
        self.perp
    }

    /// The block the books were read at.
    pub(crate) fn block(&self) -> BlockContext {
        self.block
    }

    /// The books.
    pub(crate) fn solvency(&self) -> SolvencyState {
        self.solvency
    }
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

/// One position's stored row, whichever build the market runs: its two
/// legs, its settled margin and its funding checkpoint, in the chain's
/// units. The two earlier builds' row also holds the liquidation and
/// backstop ratios the position stored at open; `v1.0.0` reads both live
/// from the margin-ratio module, so its row has none.
///
/// On V1 a position with a holder can have zero margin and zero legs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PositionRow {
    /// The perp leg: `amount0` of the stored `BalanceDelta`.
    pub perp: PerpDelta,
    /// The USD leg: `amount1` of the stored `BalanceDelta`.
    pub usd: UsdcDelta,
    /// The settled margin.
    pub margin: UsdcAtoms,
    /// The market's funding cumulative at the position's last settle,
    /// `lastCumlFundingX96`.
    pub last_cuml_funding: Funding,
    /// The ratios stored at open, on build `58b42b7` and `v0.2.2`; `None`
    /// on `v1.0.0`.
    pub stored_margin_ratios: Option<StoredMarginRatios>,
}

/// The two margin ratios a position of build `58b42b7` or `v0.2.2` stored
/// at open, each the contract's `uint24` at 1e6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMarginRatios {
    /// `liqMarginRatio`: equity over value below which it is liquidatable.
    pub liquidation: Ratio,
    /// `backstopMarginRatio`: equity over value below which it can be
    /// backstopped.
    pub backstop: Ratio,
}

impl PositionRow {
    /// The two earlier builds identify inactive positions by their empty economic fields.
    pub(super) fn held(self) -> Option<Self> {
        (self.margin != UsdcAtoms::ZERO
            || self.perp != PerpDelta::ZERO
            || self.usd != UsdcDelta::ZERO)
            .then_some(self)
    }
}

fn v1_position_calls(pos_id: U256) -> [Vec<u8>; 2] {
    [
        v1::Perp::positionsCall { posId: pos_id }.abi_encode(),
        v1::Perp::positionOwnerCall { posId: pos_id }.abi_encode(),
    ]
}

fn decode_v1_position(
    pos_id: U256,
    [row, owner]: &[IMulticall3::Result; 2],
) -> Result<Option<PositionRow>> {
    let position = decode_exact_row::<v1::Perp::positionsCall>(pos_id, row, "position")?;
    let holder = decode_row::<v1::Perp::positionOwnerCall>(pos_id, owner, "owner")?;
    Ok((holder != Address::ZERO).then(|| position.into()))
}

impl MarketReader {
    pub(super) async fn v1_position(
        &self,
        pos_id: U256,
        block: BlockId,
    ) -> Result<Option<PositionRow>> {
        let rows = self.read_rows(&[pos_id], v1_position_calls, block).await?;
        decode_v1_position(
            pos_id,
            rows.as_slice()
                .try_into()
                .expect("read_rows checked the result count"),
        )
    }

    async fn read_rows<const CALLS: usize>(
        &self,
        pos_ids: &[U256],
        encode: impl Fn(U256) -> [Vec<u8>; CALLS],
        block: BlockId,
    ) -> Result<Vec<IMulticall3::Result>> {
        let calls = pos_ids
            .iter()
            .flat_map(|&pos_id| encode(pos_id))
            .map(|calldata| IMulticall3::Call3 {
                target: self.perp,
                allowFailure: true,
                callData: calldata.into(),
            })
            .collect();
        let rows = IMulticall3::new(MULTICALL3, self.chain.provider())
            .aggregate3(calls)
            .block(block)
            .call()
            .await?;
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
}

/// The two earlier builds' row, with its stored ratios.
impl From<Position> for PositionRow {
    fn from(position: Position) -> Self {
        let (perp, usd) = unpack_balance_delta(position.delta);
        Self {
            perp: PerpDelta::new(perp),
            usd: UsdcDelta::new(usd),
            margin: UsdcAtoms::new(position.margin),
            last_cuml_funding: Funding::from_x96(position.lastCumlFundingX96),
            stored_margin_ratios: Some(StoredMarginRatios {
                liquidation: stored_ratio(position.liqMarginRatio),
                backstop: stored_ratio(position.backstopMarginRatio),
            }),
        }
    }
}

/// A stored `uint24` ratio. Every `uint24` is in [`Ratio`]'s domain.
fn stored_ratio(e6: Uint<24, 1>) -> Ratio {
    Ratio::from_e6(u24_to_u32(e6)).expect("every uint24 is a ratio")
}

/// The `v1.0.0` row: three words, no ratios.
impl From<v1::Position> for PositionRow {
    fn from(position: v1::Position) -> Self {
        let (perp, usd) = unpack_balance_delta(position.delta);
        Self {
            perp: PerpDelta::new(perp),
            usd: UsdcDelta::new(usd),
            margin: UsdcAtoms::new(position.margin),
            last_cuml_funding: Funding::from_x96(position.lastCumlFundingX96),
            stored_margin_ratios: None,
        }
    }
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

/// A failure of one chunk's shared read (its row multicall, a storage
/// batch over its rows, or the market's era before a batch), reshaped so
/// every id it served can carry it as its own error. A transport cause is
/// kept behind an `Arc` so the shared error stays retryable
/// ([`PerpCityError::is_transient`]) on every affected id; the two answers
/// a node gives about the block itself keep their typed, block-naming
/// variants, and an unknown build stays `UnknownBuild`.
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
    UnknownBuild {
        perp: Address,
        hook: Address,
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
            PerpCityError::Contract(ContractError::UnknownBuild { perp, hook }) => {
                Self::UnknownBuild { perp, hook }
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
            Self::UnknownBuild { perp, hook } => ContractError::UnknownBuild {
                perp: *perp,
                hook: *hook,
            },
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

    /// State reads pinned to the head block the endpoint serves now: one a
    /// transaction this process saw confirmed is in, which the lagged
    /// snapshot block does not promise. Not reorg-safe; a read that must
    /// survive a reorg takes [`Self::state`].
    ///
    /// # Errors
    ///
    /// [`ContractError::BlockUnavailable`] when the serving replica is
    /// missing the header it just named.
    pub async fn state_at_head(&self) -> Result<StateAt> {
        let head = self.chain.provider().get_block_number().await?;
        self.state_at(head).await
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

    /// `calldata` from `from` to the perp, as an `eth_call` at this block:
    /// the contract's answer to that call against this block's state.
    ///
    /// # Errors
    ///
    /// The contract's revert as [`TransactionError::SimulationReverted`];
    /// a node without this block or its state as
    /// [`ContractError::BlockUnavailable`] (transient) or
    /// [`ContractError::StateUnavailable`], as a pinned read types them;
    /// any other failure as [`classify_simulation_failure`] sorts it.
    pub(super) async fn simulate_from(&self, from: Address, calldata: &Bytes) -> Result<()> {
        let tx = preflight_request(from, self.market.perp, calldata, 0, None);
        self.market
            .chain
            .provider()
            .call(tx)
            .block(self.id())
            .await
            .map_err(|e| -> PerpCityError {
                let failure = classify_simulation_failure(&e, "eth_call");
                if matches!(failure, TransactionError::SimulationReverted { .. }) {
                    return failure.into();
                }
                match e
                    .as_error_resp()
                    .and_then(|payload| pinned_block_failure(&payload.message, self.block.number))
                {
                    Some(block) => block.into(),
                    None => failure.into(),
                }
            })?;
        Ok(())
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

    /// The solvency books at this handle's block, stamped with it.
    ///
    /// # Errors
    ///
    /// As [`Self::solvency`].
    pub async fn books(&self) -> Result<Books> {
        Ok(Books {
            perp: self.market.perp,
            block: self.block,
            solvency: self.solvency().await?,
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

    /// This market's build, from the cached immutables: the pool key's hook
    /// against the chain's deployments, read once per market at the head
    /// (a build is fixed for a market's life, so no block is needed).
    ///
    /// # Errors
    ///
    /// [`ContractError::UnknownBuild`] for a hook the chain's deployments
    /// do not name; the transport error from either read.
    async fn era(&self) -> Result<Era> {
        Ok(self.market.immutables().await?.era)
    }

    pub(super) async fn require_read_era(&self, v1: bool, operation: &'static str) -> Result<()> {
        let era = self.era().await?;
        if (era == Era::V1) != v1 {
            return Err(ContractError::UnsupportedEra { era, operation }.into());
        }
        Ok(())
    }

    /// The build for a batch over `pos_ids`, or the batch's whole answer
    /// when there is nothing to read: no outcome for no ids, with no
    /// request, and the era read's failure on every id when it fails.
    async fn batch_era<T>(&self, pos_ids: &[U256]) -> std::result::Result<Era, Vec<RowOutcome<T>>> {
        if pos_ids.is_empty() {
            return Err(Vec::new());
        }
        self.era().await.map_err(|e| {
            tracing::debug!(ids = pos_ids.len(), error = %e, "era read failed");
            let failure = ChunkReadFailure::new("era read", e);
            pos_ids
                .iter()
                .map(|&pos_id| RowOutcome {
                    pos_id,
                    row: Err(failure.error()),
                })
                .collect()
        })
    }

    /// One position's stored row in the shape of the market's build, or
    /// `None` for an id that was never minted or has been closed (every
    /// build reports both as an empty row).
    ///
    /// `None` rather than an error because this is the read for
    /// enumerating `1..next_pos_id()`, where most ids are closed;
    /// [`MarketReader::get_position`] asks about a position expected to
    /// exist and fails with `PositionNotFound` instead.
    ///
    /// # Errors
    ///
    /// [`ContractError::UnknownBuild`] for an unknown build, before the
    /// row is read; the pinned read's error.
    pub async fn position(&self, pos_id: U256) -> Result<Option<PositionRow>> {
        let provider = self.market.chain.provider();
        let row = match self.era().await? {
            Era::Legacy | Era::Upgradeable => PositionRow::from(
                Perp::new(self.market.perp, provider)
                    .positions(pos_id)
                    .block(self.id())
                    .call()
                    .await
                    .map_err(|e| self.read_error(e))?,
            ),
            Era::V1 => {
                return self
                    .market
                    .v1_position(pos_id, self.id())
                    .await
                    .map_err(|e| pinned_read_error(e, self.block.number));
            }
        };
        Ok(row.held())
    }

    /// The stored rows of many positions, in one row multicall per
    /// [`MAX_ROW_BATCH`] ids, all at this block, each in the shape of the
    /// market's build.
    ///
    /// Exactly one [`RowOutcome`] per input id, in input order, with
    /// [`Self::position`]'s reading of each row: `None` for an id that was
    /// never minted or has been closed. A row that reverts or does not
    /// decode fails alone; a chunk whose multicall fails marks every id of
    /// that chunk failed with the shared cause, and the other chunks stand;
    /// an era read that fails, or an unknown build, marks every id failed.
    /// Nothing fails the batch, so a sweep over `1..next_pos_id()` always
    /// gets an answer for every id.
    pub async fn positions(&self, pos_ids: &[U256]) -> Vec<RowOutcome<PositionRow>> {
        match self.batch_era(pos_ids).await {
            Err(outcomes) => outcomes,
            Ok(Era::Legacy | Era::Upgradeable) => {
                self.rows(
                    pos_ids,
                    |pos_id| [Perp::positionsCall { posId: pos_id }.abi_encode()],
                    |pos_id, [row]| {
                        let position = decode_row::<Perp::positionsCall>(pos_id, row, "position")?;
                        Ok(PositionRow::from(position).held())
                    },
                )
                .await
            }
            Ok(Era::V1) => {
                self.rows(pos_ids, v1_position_calls, decode_v1_position)
                    .await
            }
        }
    }

    /// The holder of each position, in one row multicall per
    /// [`MAX_ROW_BATCH`] ids, all at this block: exactly one
    /// [`RowOutcome`] per id, in input order.
    ///
    /// `Ok(None)` is an id never minted or closed. On the two earlier
    /// builds that is the position NFT's own `TokenDoesNotExist` revert of
    /// `ownerOf`, and any other revert fails that id alone, so a row that
    /// ran out of gas is never read as burned. A `v1.0.0` market has no
    /// NFT: its `positionOwner` answers the zero address for an inactive
    /// id, and any revert fails that id. An era read that fails, or an
    /// unknown build, marks every id failed.
    pub async fn owners(&self, pos_ids: &[U256]) -> Vec<RowOutcome<Address>> {
        match self.batch_era(pos_ids).await {
            Err(outcomes) => outcomes,
            Ok(Era::Legacy | Era::Upgradeable) => {
                self.rows(
                    pos_ids,
                    |pos_id| [Perp::ownerOfCall { tokenId: pos_id }.abi_encode()],
                    |pos_id, [row]| {
                        if !row.success && row.returnData[..] == Perp::TokenDoesNotExist::SELECTOR {
                            return Ok(None);
                        }
                        decode_row::<Perp::ownerOfCall>(pos_id, row, "owner").map(Some)
                    },
                )
                .await
            }
            Ok(Era::V1) => {
                self.rows(
                    pos_ids,
                    |pos_id| [v1::Perp::positionOwnerCall { posId: pos_id }.abi_encode()],
                    |pos_id, [row]| {
                        let owner =
                            decode_row::<v1::Perp::positionOwnerCall>(pos_id, row, "owner")?;
                        Ok((owner != Address::ZERO).then_some(owner))
                    },
                )
                .await
            }
        }
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
        self.market
            .read_rows(pos_ids, encode, self.id())
            .await
            .map_err(|e| pinned_read_error(e, self.block.number))
    }

    /// One position's band, or `None` if it holds no liquidity there: a
    /// taker, or a maker whose liquidity is gone. Only the band's ticks and
    /// liquidity are read from the build's `makerDetails`, the first three
    /// fields on every build.
    ///
    /// The chain's ticks pass through [`TickRange::new`], so this is where
    /// a malformed range would fail — never the math downstream.
    ///
    /// # Errors
    ///
    /// [`ContractError::UnknownBuild`] for an unknown build, before the
    /// row is read; the pinned read's error; a malformed range.
    pub async fn maker_band(&self, pos_id: U256) -> Result<Option<MakerBand>> {
        let provider = self.market.chain.provider();
        match self.era().await? {
            Era::Legacy | Era::Upgradeable => {
                let maker = Perp::new(self.market.perp, provider)
                    .makerDetails(pos_id)
                    .block(self.id())
                    .call()
                    .await
                    .map_err(|e| self.read_error(e))?;
                band(maker.tickLower, maker.tickUpper, maker.liquidity)
            }
            Era::V1 => {
                let data = v1::Perp::new(self.market.perp, provider)
                    .makerDetails(pos_id)
                    .block(self.id())
                    .call_raw()
                    .await
                    .map_err(|e| self.read_error(e))?;
                let maker =
                    decode_exact::<v1::Perp::makerDetailsCall>(pos_id, &data, "maker details")?;
                band(maker.tickLower, maker.tickUpper, maker.liquidity)
            }
        }
    }

    /// [`Self::maker_band`] for many ids, in one row multicall per
    /// [`MAX_ROW_BATCH`] ids, all at this block: exactly one
    /// [`RowOutcome`] per id, `Ok(None)` for an id that holds no liquidity.
    /// An era read that fails, or an unknown build, marks every id failed.
    pub async fn maker_bands(&self, pos_ids: &[U256]) -> Vec<RowOutcome<MakerBand>> {
        match self.batch_era(pos_ids).await {
            Err(outcomes) => outcomes,
            Ok(Era::Legacy | Era::Upgradeable) => {
                self.rows(
                    pos_ids,
                    |pos_id| [Perp::makerDetailsCall { posId: pos_id }.abi_encode()],
                    |pos_id, [row]| {
                        let maker =
                            decode_row::<Perp::makerDetailsCall>(pos_id, row, "maker details")?;
                        band(maker.tickLower, maker.tickUpper, maker.liquidity)
                    },
                )
                .await
            }
            Ok(Era::V1) => {
                self.rows(
                    pos_ids,
                    |pos_id| [v1::Perp::makerDetailsCall { posId: pos_id }.abi_encode()],
                    |pos_id, [row]| {
                        let maker = decode_exact_row::<v1::Perp::makerDetailsCall>(
                            pos_id,
                            row,
                            "maker details",
                        )?;
                        band(maker.tickLower, maker.tickUpper, maker.liquidity)
                    },
                )
                .await
            }
        }
    }

    /// Read the v1 fee module and trade rates at this handle's block.
    pub(super) async fn taker_fee_rates(&self) -> Result<TakerFeeRates> {
        let perp = v1::Perp::new(self.market.perp, self.market.chain.provider());
        let modules = perp
            .modules()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let fees = v1::IFees::new(
            registered_module(modules.fees, "IFees")?,
            self.market.chain.provider(),
        );
        let (max_swap_fee, protocol_fee) = self
            .market
            .chain
            .multicall_at(self.id())
            .add(fees.maxSwapFee())
            .add(perp.protocolFee())
            .aggregate()
            .await
            .map_err(|e| self.multicall_read_error(e))?;
        let ratio = |e6: U256, name: &str| {
            u32::try_from(e6)
                .map_err(|_| ValidationError::Overflow {
                    context: format!("{name} {e6} exceeds uint32"),
                })
                .and_then(Ratio::from_e6)
        };
        Ok(TakerFeeRates::new(
            ratio(max_swap_fee, "maxSwapFee")?,
            ratio(protocol_fee, "protocolFee")?,
        ))
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

    /// The market's accumulators at this block on the two earlier builds:
    /// `cumulatives()`, as the `CumulativesAccrued` event carries them.
    ///
    /// # Errors
    ///
    /// [`ContractError::UnsupportedEra`] on a `v1.0.0` market, before any
    /// read: its eight words decode as the earlier six without an error and
    /// into the wrong fields, so its accumulators are
    /// [`Self::v1_cumulatives`]; [`ContractError::UnknownBuild`] for an
    /// unknown build.
    pub async fn cumulatives(&self) -> Result<CumulativesInfo> {
        if self.era().await? == Era::V1 {
            return Err(ContractError::UnsupportedEra {
                era: Era::V1,
                operation: "cumulatives",
            }
            .into());
        }
        Perp::new(self.market.perp, self.market.chain.provider())
            .cumulatives()
            .block(self.id())
            .call()
            .await
            .map(Into::into)
            .map_err(|e| self.read_error(e))
    }

    /// A `v1.0.0` market's accumulators at this block: its `cumulatives()`.
    ///
    /// # Errors
    ///
    /// [`ContractError::UnsupportedEra`] on an earlier build, before any
    /// read; [`ContractError::UnknownBuild`] for an unknown build; the
    /// pinned read's error.
    pub async fn v1_cumulatives(&self) -> Result<V1Cumulatives> {
        let era = self.era().await?;
        if era != Era::V1 {
            return Err(ContractError::UnsupportedEra {
                era,
                operation: "v1_cumulatives",
            }
            .into());
        }
        v1::Perp::new(self.market.perp, self.market.chain.provider())
            .cumulatives()
            .block(self.id())
            .call()
            .await
            .map(Into::into)
            .map_err(|e| self.read_error(e))
    }

    /// The stored EMA pair and the touch it is current as of, at this
    /// block: the pair from its storage word (slot 11 on the earlier
    /// builds, 23 on `v1.0.0`), the touch from `rates()`. The same `Emas`
    /// the `RatesAndEmasRefreshed` event carries, before any advance to a
    /// later timestamp; [`Self::mark`] is the advanced form.
    ///
    /// # Errors
    ///
    /// [`ContractError::UnknownBuild`] for an unknown build; the pinned
    /// read's error.
    pub async fn emas(&self) -> Result<Emas> {
        let chain = &self.market.chain;
        let era = self.era().await?;
        let (rates, stored) = tokio::try_join!(self.rates(), async {
            let stored = match era {
                Era::Legacy | Era::Upgradeable => {
                    chain.stored_emas_at(self.market.perp, self.id()).await
                }
                Era::V1 => chain.v1_stored_emas_at(self.market.perp, self.id()).await,
            };
            stored.map_err(|e| pinned_read_error(e, self.block.number))
        })?;
        Ok(Emas {
            amm_price: Price::from_x96(U256::from(stored.amm)),
            index: Price::from_x96(U256::from(stored.index)),
            last_touch: rates.last_touch,
        })
    }

    /// The pool's current tick.
    pub async fn pool_tick(&self) -> Result<i32> {
        Ok(i24_to_i32(self.pool_state().await?.tick))
    }

    /// The pool's current sqrt price at this block.
    pub async fn pool_sqrt_price(&self) -> Result<SqrtPrice> {
        Ok(SqrtPrice::from_x96(
            self.pool_state().await?.sqrtPrice.to::<U256>(),
        ))
    }

    async fn pool_state(&self) -> Result<Perp::poolStateReturn> {
        let perp = Perp::new(self.market.perp, self.market.chain.provider());
        perp.poolState()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))
    }

    pub(super) async fn v1_open_interest(&self) -> Result<PerSide<PerpAtoms>> {
        let oi = v1::Perp::new(self.market.perp, self.market.chain.provider())
            .openInterest()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        Ok(PerSide::new(
            PerpAtoms::new(oi.long),
            PerpAtoms::new(oi.short),
        ))
    }

    /// The USDC the perp holds, which backs its positions' margin and also
    /// its insurance fund and uncollected fees.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] if the balance does not fit `u128`.
    pub async fn collateral(&self) -> Result<UsdcAtoms> {
        let chain = &self.market.chain;
        let raw: U256 = IERC20::new(chain.deployments().usdc, chain.provider())
            .balanceOf(self.market.perp)
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        Ok(UsdcAtoms::try_from(raw)?)
    }

    /// The market's taker capacity and the open interest drawing on it, in
    /// one multicall; on a `v1.0.0` market with its physical close
    /// capacity in the same batch.
    ///
    /// [`MarketCapacity`] derives each side's headroom and utilization as
    /// the market's build computes them.
    ///
    /// # Errors
    ///
    /// [`ContractError::UnknownBuild`] for an unknown build; the pinned
    /// read's error.
    pub async fn capacity(&self) -> Result<MarketCapacity> {
        if self.era().await? == Era::V1 {
            return self.v1_capacity().await;
        }
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
            close_capacity: None,
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
    /// beacon index, and the stored EMAs advanced to the block timestamp,
    /// with the mark the market's pricing module makes of them.
    /// The views, index and stored EMAs are pinned to this block. On v1,
    /// the market reads and checks the index through `readBeaconIndex`.
    /// Its pricing module prices the accepted index and advanced EMAs.
    ///
    /// # Errors
    ///
    /// [`ContractError::ModuleNotRegistered`] when the perp has no beacon
    /// (or, on `v1.0.0`, no pricing module); [`ContractError::UnknownBuild`]
    /// for an unknown build.
    pub async fn mark(&self) -> Result<Mark> {
        if self.era().await? == Era::V1 {
            let views = self.v1_views().await?;
            return Ok(self.v1_mark_from(&views).await?.0);
        }
        let views = self.perp_views().await?;
        self.mark_from(&views).await
    }

    /// The raw beacon observation and the v1 market's latency at this block.
    /// A caller-sensitive beacon can return a different index to the market.
    /// One multicall for `modules()` and
    /// `latency()`, then the beacon's `index()`, both pinned here.
    ///
    /// # Errors
    ///
    /// [`ContractError::UnsupportedEra`] on an earlier build, whose beacon
    /// returns no update time; [`ContractError::ModuleNotRegistered`] when
    /// the perp has no beacon; [`ContractError::UnknownBuild`] for an
    /// unknown build; the pinned read's error.
    pub async fn beacon_index(&self) -> Result<BeaconIndex> {
        let era = self.era().await?;
        if era != Era::V1 {
            return Err(ContractError::UnsupportedEra {
                era,
                operation: "beacon_index",
            }
            .into());
        }
        let perp = v1::Perp::new(self.market.perp, self.market.chain.provider());
        let (modules, latency) = self
            .market
            .chain
            .multicall_at(self.id())
            .add(perp.modules())
            .add(perp.latency())
            .aggregate()
            .await
            .map_err(|e| self.multicall_read_error(e))?;
        let beacon = registered_module(modules.beacon, "IBeacon")?;
        let (index, updated_at) = self
            .market
            .chain
            .v1_index_at(beacon, self.id())
            .await
            .map_err(|e| pinned_read_error(e, self.block.number))?;
        if index.is_zero() {
            return Err(ValidationError::InvalidPrice {
                reason: "beacon returned zero index".into(),
            }
            .into());
        }
        let updated_at = u64::try_from(updated_at).map_err(|_| ValidationError::Overflow {
            context: "beacon updatedAt exceeds u64".into(),
        })?;
        Ok(BeaconIndex {
            block: self.block,
            index: Price::from_x96(index),
            updated_at,
            latency,
        })
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
        let (pool, (impact_sqrt_min, impact_sqrt_max)) = match immutables.era {
            Era::Legacy | Era::Upgradeable => {
                let views = self.perp_views().await?;
                let mark = self.mark_from(&views).await?;
                let bounds = self.impact_bounds(views.modules.priceImpact, &mark).await?;
                let pool = &views.pool_state;
                (
                    (pool.tick, pool.sqrtPrice, pool.liquidity),
                    (bounds.sqrtMin, bounds.sqrtMax),
                )
            }
            Era::V1 => {
                let views = self.v1_views().await?;
                let (mark, _) = self.v1_mark_from(&views).await?;
                let bounds = self.v1_impact_bounds(&views, &mark).await?;
                let pool = &views.pool_state;
                (
                    (pool.tick, pool.sqrtPrice, pool.liquidity),
                    (bounds.sqrtMin, bounds.sqrtMax),
                )
            }
        };
        let ticks = self.tick_map(&immutables).await?;

        let (tick, sqrt_price, liquidity) = pool;
        let tick = i24_to_i32(tick);
        let reconstructed = active_liquidity(&ticks, tick)?;
        if reconstructed != LUnits::new(liquidity) {
            return Err(ContractError::StorageReadFailed {
                context: format!(
                    "tick map liquidity mismatch: reconstructed {}, pool {}",
                    reconstructed.units(),
                    liquidity
                ),
                source: None,
            }
            .into());
        }
        let (protocol_sqrt_min, protocol_sqrt_max) = immutables.era.swap_sqrt_price_limits();
        Ok(PoolSnapshot {
            block: self.block,
            sqrt_price: SqrtPrice::from_x96(sqrt_price.to::<U256>()),
            tick,
            liquidity: LUnits::new(liquidity),
            ticks,
            protocol_sqrt_min,
            protocol_sqrt_max,
            impact_sqrt_min: SqrtPrice::from_x96(impact_sqrt_min),
            impact_sqrt_max: SqrtPrice::from_x96(impact_sqrt_max),
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

    /// A `v1.0.0` market's mark-input views in one multicall at this block:
    /// `modules()`, `poolState()`, `rates()` and `emaWindow()`.
    pub(super) async fn v1_views(&self) -> Result<V1Views> {
        let perp = v1::Perp::new(self.market.perp, self.market.chain.provider());
        let (modules, pool_state, rates, ema_window) = self
            .market
            .chain
            .multicall_at(self.id())
            .add(perp.modules())
            .add(perp.poolState())
            .add(perp.rates())
            .add(perp.emaWindow())
            .aggregate()
            .await
            .map_err(|e| self.multicall_read_error(e))?;
        Ok(V1Views {
            modules,
            pool_state,
            last_touch: rates.lastTouch.to::<u64>(),
            ema_window: u64::from(u24_to_u32(ema_window)),
        })
    }

    /// A `v1.0.0` market's mark from its views, as `_previewAccrue` makes
    /// it: the beacon's index and the stored EMAs (slot 23) read together
    /// at this block, the pair advanced to the block's timestamp, then the
    /// market's pricing module's `fairPrice` of the spots and the advanced
    /// pair by `eth_call` here. The stored pair comes back beside it.
    pub(super) async fn v1_mark_from(&self, views: &V1Views) -> Result<(Mark, PricePair)> {
        let beacon = registered_module(views.modules.beacon, "IBeacon")?;
        let pricing = registered_module(views.modules.pricing, "IPricing")?;
        let chain = &self.market.chain;
        let (index, stored) = tokio::try_join!(
            chain.v1_market_index_at(self.market.perp, beacon, self.id()),
            chain.v1_stored_emas_at(self.market.perp, self.id()),
        )
        .map_err(|e| pinned_read_error(e, self.block.number))?;
        let pool_price = Price::from_x96(views.pool_state.ammPrice);
        let index = Price::from_x96(index);
        let spot = PricePair::try_from_x96(pool_price.x96(), index.x96())?;
        let emas = calculate_emas(
            stored,
            spot,
            views.last_touch,
            self.block.timestamp,
            views.ema_window,
        )?;
        let price = v1::IPricing::new(pricing, chain.provider())
            .fairPrice(
                pool_price.x96(),
                index.x96(),
                U256::from(emas.amm),
                U256::from(emas.index),
            )
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let mark = Mark {
            block: self.block,
            pool_price,
            index,
            emas,
            price: Price::from_x96(price),
        };
        Ok((mark, stored))
    }

    /// A `v1.0.0` price-impact module's swap bounds at this mark: its
    /// five-argument `sqrtPriceBounds`, whose last argument is the mark.
    async fn v1_impact_bounds(
        &self,
        views: &V1Views,
        mark: &Mark,
    ) -> Result<v1::IPriceImpact::sqrtPriceBoundsReturn> {
        let module = registered_module(views.modules.priceImpact, "IPriceImpact")?;
        v1::IPriceImpact::new(module, self.market.chain.provider())
            .sqrtPriceBounds(
                mark.pool_price.x96(),
                mark.index.x96(),
                U256::from(mark.emas.amm),
                U256::from(mark.emas.index),
                mark.price.x96(),
            )
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))
    }

    /// [`Self::capacity`] on a `v1.0.0` market: `capacity()`,
    /// `openInterest()` and `physicalCloseCapacity()` in one multicall.
    async fn v1_capacity(&self) -> Result<MarketCapacity> {
        let perp = v1::Perp::new(self.market.perp, self.market.chain.provider());
        let (capacity, oi, close) = self
            .market
            .chain
            .multicall_at(self.id())
            .add(perp.capacity())
            .add(perp.openInterest())
            .add(perp.physicalCloseCapacity())
            .aggregate()
            .await
            .map_err(|e| self.multicall_read_error(e))?;
        Ok(MarketCapacity {
            block: self.block,
            capacity: Capacity::new(
                PerpAtoms::new(capacity.long),
                PerpAtoms::new(capacity.short),
            ),
            open_interest: PerSide::new(PerpAtoms::new(oi.long), PerpAtoms::new(oi.short)),
            close_capacity: Some(Capacity::new(
                PerpAtoms::new(close.longClose),
                PerpAtoms::new(close.shortClose),
            )),
        })
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

/// A stored band as a [`MakerBand`], or `None` when it holds no liquidity;
/// the ticks pass through [`TickRange::new`]. Every build's `Maker` opens
/// with these three fields.
fn band(lower: Signed<24, 1>, upper: Signed<24, 1>, liquidity: u128) -> Result<Option<MakerBand>> {
    if liquidity == 0 {
        return Ok(None);
    }
    let range = TickRange::new(i24_to_i32(lower), i24_to_i32(upper))?;
    Ok(Some(MakerBand::new(range, LUnits::new(liquidity))))
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

/// [`decode_row`] for a `v1.0.0` row, which must also be exactly its
/// return's width ([`decode_exact`]).
fn decode_exact_row<C: SolCall>(
    pos_id: U256,
    row: &IMulticall3::Result,
    what: &str,
) -> Result<C::Return> {
    if row.success {
        exact_width::<C>(pos_id, &row.returnData, what)?;
    }
    decode_row::<C>(pos_id, row, what)
}

/// `data` decoded as `C`'s return only when it is exactly that return's
/// width. The decode reads a static return's words and ignores any after
/// them, so without this check an earlier build's five-word `positions` or
/// ten-word `makerDetails` answer would read as a `v1.0.0` row. A wrong
/// width is [`ValidationError::DecodeFailed`], as is an answer that does
/// not decode.
pub(super) fn decode_exact<C: SolCall>(pos_id: U256, data: &[u8], what: &str) -> Result<C::Return> {
    exact_width::<C>(pos_id, data, what)?;
    C::abi_decode_returns(data).map_err(|e| {
        ValidationError::DecodeFailed {
            context: format!("{what} {pos_id}: {e}"),
        }
        .into()
    })
}

/// Fails unless `data` is exactly the width of `C`'s static return.
fn exact_width<C: SolCall>(pos_id: U256, data: &[u8], what: &str) -> Result<()> {
    match <C::ReturnTuple<'_> as SolType>::ENCODED_SIZE {
        Some(width) if data.len() != width => Err(ValidationError::DecodeFailed {
            context: format!(
                "{what} {pos_id}: the answer is {} bytes, the row is {width}",
                data.len()
            ),
        }
        .into()),
        _ => Ok(()),
    }
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
    match pinned_block_failure(&payload.message, number) {
        Some(block) => block.into(),
        None => error,
    }
}

/// The failure a node's error `message` names about block `number`
/// itself, as [`pinned_read_error`] types it, or `None` for any other.
pub(super) fn pinned_block_failure(message: &str, number: u64) -> Option<ContractError> {
    let message = message.to_ascii_lowercase();
    if state_pruned(&message) {
        Some(ContractError::StateUnavailable { number })
    } else if block_missing(&message) {
        Some(ContractError::BlockUnavailable { number })
    } else {
        None
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

/// A `v1.0.0` market's views a mark and the pool are built from, read
/// together at one block: [`PerpViews`] in that build's shapes.
pub(super) struct V1Views {
    /// `modules()`.
    pub(super) modules: v1::Modules,
    /// `poolState()`.
    pub(super) pool_state: v1::Perp::poolStateReturn,
    /// `rates().lastTouch`.
    pub(super) last_touch: u64,
    /// `emaWindow()`, in seconds.
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
    use crate::client::mock::{
        self, PERP, Rpc, e6, returns, v1_maker_words, v1_position_words, x96,
    };
    use crate::constants::{MAX_SWAP_SQRT_PRICE_X96, MIN_SWAP_SQRT_PRICE_X96, SNAPSHOT_BLOCK_LAG};
    use crate::contracts::{IBeacon, Modules, Rates};
    use crate::convert::pack_balance_delta;
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
    async fn state_at_head_pins_the_block_the_endpoint_names() {
        let (client, rpc) = mock::client();
        rpc.quantity(100);
        let hash = rpc.block(100, TIMESTAMP);

        let state = client.market().state_at_head().await.unwrap();
        assert_eq!((state.block().number, state.block().hash), (100, hash));
        assert!(rpc.is_drained(), "blockNumber and the header, nothing else");
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
        rpc.immutables(Era::Legacy);
        rpc.call::<Perp::positionsCall>(&mock::position(0));
        assert!(state.position(U256::from(3)).await.unwrap().is_none());

        rpc.call::<Perp::positionsCall>(&mock::position(5_000_000));
        let position = state.position(U256::from(4)).await.unwrap().unwrap();
        assert_eq!(position.margin, UsdcAtoms::new(5_000_000));

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
        rpc.immutables(Era::Legacy);
        let mut exposed = mock::position(0);
        exposed.delta = I256::try_from(-7).unwrap();
        rpc.call::<Perp::positionsCall>(&exposed);
        assert!(state.position(U256::from(9)).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn maker_band_is_none_without_liquidity() {
        let (state, rpc) = state().await;
        rpc.immutables(Era::Legacy);
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
        rpc.immutables(Era::Legacy);
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
    async fn pool_sqrt_price_is_the_slot_price() {
        let (state, rpc) = state().await;
        let mut slot = mock::pool_state_at_tick(38_340);
        slot.sqrtPrice = (U256::from(7u8) << 96usize).to();
        rpc.call::<Perp::poolStateCall>(&slot);
        assert_eq!(
            state.pool_sqrt_price().await.unwrap(),
            SqrtPrice::from_x96(U256::from(7u8) << 96)
        );
        assert!(rpc.is_drained());
    }

    #[tokio::test]
    async fn collateral_is_the_perps_usdc_balance() {
        let (state, rpc) = state().await;
        rpc.call::<IERC20::balanceOfCall>(&U256::from(3_265_080_000u64));
        assert_eq!(
            state.collateral().await.unwrap(),
            UsdcAtoms::new(3_265_080_000)
        );
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
        rpc.immutables(Era::Legacy);
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
        rpc.immutables(Era::Legacy);
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
        rpc.immutables(Era::Legacy);
        rpc.fails("historical state 1682db2b is not available");

        let error = state.capacity().await.unwrap_err();
        let PerpCityError::Contract(ContractError::StateUnavailable { number }) = error else {
            panic!("a multicall over pruned state must type as StateUnavailable, got {error}");
        };
        assert_eq!(number, 92);
        assert!(!error.is_transient());
    }

    // ── A position's row, by build ────────────────────────────────────

    /// The `v1.0.0` row is three words, and the earlier builds' five-word
    /// binding cannot read it: decoded with the old shape it is a loud
    /// failure, never a row with garbage ratios.
    #[test]
    fn a_v1_position_is_three_words() {
        let words = v1_position_words(-2_500_000, 3_000_000, 7_000_000, -42);
        assert_eq!(words.len(), 3 * 32);
        let position = v1::Perp::positionsCall::abi_decode_returns(&words).unwrap();
        assert_eq!(position.margin, 7_000_000);
        assert_eq!(position.lastCumlFundingX96, I256::try_from(-42).unwrap());
        assert_eq!(
            PositionRow::from(position),
            PositionRow {
                perp: PerpDelta::new(-2_500_000),
                usd: UsdcDelta::new(3_000_000),
                margin: UsdcAtoms::new(7_000_000),
                last_cuml_funding: Funding::from_x96(I256::try_from(-42).unwrap()),
                stored_margin_ratios: None,
            }
        );
        assert!(Perp::positionsCall::abi_decode_returns(&words).is_err());
    }

    /// The earlier builds' row keeps its two stored ratios.
    #[test]
    fn an_earlier_build_s_row_keeps_its_stored_ratios() {
        let position = Position {
            delta: pack_balance_delta(400_000, -1_000_000),
            margin: 10_000_000,
            liqMarginRatio: e6(50_000),
            backstopMarginRatio: e6(20_000),
            lastCumlFundingX96: I256::try_from(77).unwrap(),
        };
        assert_eq!(
            PositionRow::from(position),
            PositionRow {
                perp: PerpDelta::new(400_000),
                usd: UsdcDelta::new(-1_000_000),
                margin: UsdcAtoms::new(10_000_000),
                last_cuml_funding: Funding::from_x96(I256::try_from(77).unwrap()),
                stored_margin_ratios: Some(StoredMarginRatios {
                    liquidation: Ratio::from_e6(50_000).unwrap(),
                    backstop: Ratio::from_e6(20_000).unwrap(),
                }),
            }
        );
    }

    /// The `v1.0.0` maker is seven words with the band in the first three;
    /// the earlier builds' ten-word binding cannot read it.
    #[test]
    fn a_v1_maker_is_seven_words() {
        let words = v1_maker_words(-600, 1_200, 335_000_000);
        assert_eq!(words.len(), 7 * 32);
        let maker = v1::Perp::makerDetailsCall::abi_decode_returns(&words).unwrap();
        assert_eq!(
            (
                maker.tickLower.as_i32(),
                maker.tickUpper.as_i32(),
                maker.liquidity
            ),
            (-600, 1_200, 335_000_000)
        );
        assert_eq!(maker.lastLpFeeGrowthInsideX128, U256::from(7));
        assert_eq!(maker.lastCumlFunding.belowX96, I256::try_from(-8).unwrap());
        assert_eq!(
            maker.lastCumlFunding.withinInventoryX192,
            I256::try_from(9).unwrap()
        );
        assert_eq!(
            maker.lastFundingCorrectionX128,
            I256::try_from(-10).unwrap()
        );
        assert!(Perp::makerDetailsCall::abi_decode_returns(&words).is_err());
    }

    /// The market's hook names the shape: the two earlier builds read the
    /// five-word row, `v1.0.0` the three-word one. Each read of the other's
    /// answer fails loudly: the earlier binding cannot decode three words,
    /// and the `v1.0.0` read refuses any answer that is not exactly three
    /// words ([`decode_exact`], tested in
    /// `v1_row_reads_refuse_an_earlier_build_s_answer`). One era read per
    /// market, then the row.
    #[tokio::test]
    async fn position_reads_the_row_of_the_market_s_build() {
        let v1_row = v1_position_words(-2_500_000, 3_000_000, 7_000_000, 0);
        for era in [Era::Legacy, Era::Upgradeable] {
            let (state, rpc) = state().await;
            rpc.immutables(era);
            rpc.call::<Perp::positionsCall>(&mock::position(5_000_000));
            let row = state.position(U256::from(4)).await.unwrap().unwrap();
            assert_eq!(row.margin, UsdcAtoms::new(5_000_000));
            assert!(row.stored_margin_ratios.is_some(), "{era:?}");

            rpc.raw(&v1_row);
            let err = state.position(U256::from(4)).await.unwrap_err();
            assert!(
                matches!(err, PerpCityError::Abi(_)),
                "{era:?}: a v1 row on the old shape is loud: {err}"
            );
            assert!(rpc.is_drained(), "poolKey, POOL_ID, then one row per read");
        }

        let (state, rpc) = state().await;
        rpc.immutables(Era::V1);
        rpc.aggregate3(vec![mock::ok_row(v1_row), owner_row(PERP)]);
        let row = state.position(U256::from(4)).await.unwrap().unwrap();
        assert_eq!(
            (row.perp, row.usd, row.margin, row.stored_margin_ratios),
            (
                PerpDelta::new(-2_500_000),
                UsdcDelta::new(3_000_000),
                UsdcAtoms::new(7_000_000),
                None
            )
        );
        // A closed v1 taker: the contract deletes the row.
        rpc.aggregate3(vec![
            mock::ok_row(v1_position_words(0, 0, 0, 0)),
            owner_row(Address::ZERO),
        ]);
        assert!(state.position(U256::from(5)).await.unwrap().is_none());
        assert_eq!(
            rpc.selectors(),
            [
                Perp::poolKeyCall::SELECTOR,
                v1::Perp::poolIdCall::SELECTOR,
                v1::Perp::positionsCall::SELECTOR,
                v1::Perp::positionOwnerCall::SELECTOR,
                v1::Perp::positionsCall::SELECTOR,
                v1::Perp::positionOwnerCall::SELECTOR,
            ]
        );
        assert!(rpc.is_drained());
    }

    fn owner_row(owner: Address) -> IMulticall3::Result {
        mock::ok_row(returns::<v1::Perp::positionOwnerCall>(&owner))
    }

    #[tokio::test]
    async fn v1_single_zero_rows_follow_the_owner() {
        let (state, rpc) = state().await;
        rpc.immutables(Era::V1);
        for latest in [true, false] {
            for owner in [Some(PERP), None, Some(Address::ZERO)] {
                rpc.aggregate3(vec![
                    mock::ok_row(v1_position_words(0, 0, 0, 0)),
                    owner.map_or_else(mock::failed_row, owner_row),
                ]);
                let result = if latest {
                    state.market().get_position(U256::ONE).await.map(Some)
                } else {
                    state.position(U256::ONE).await
                };
                match owner {
                    None => assert!(matches!(
                        result,
                        Err(PerpCityError::Contract(
                            ContractError::MulticallFailed { .. }
                        ))
                    )),
                    Some(a) if a == Address::ZERO && latest => assert!(matches!(
                        result,
                        Err(PerpCityError::Contract(
                            ContractError::PositionNotFound { .. }
                        ))
                    )),
                    Some(a) if a == Address::ZERO => assert!(result.unwrap().is_none()),
                    Some(_) => assert_eq!(result.unwrap().unwrap().margin, UsdcAtoms::ZERO),
                }
            }
        }
        assert!(rpc.is_drained());
    }

    /// Whether a read failed as a row that does not decode.
    fn decode_failed<T>(read: &Result<T>) -> bool {
        matches!(
            read,
            Err(PerpCityError::Validation(
                ValidationError::DecodeFailed { .. }
            ))
        )
    }

    /// Alloy's decode of a static return reads its words and ignores any
    /// after them, so an earlier build's longer answer decodes as a
    /// `v1.0.0` row without error. The `v1.0.0` rows are therefore read
    /// only at their exact width, three words for `positions` and seven
    /// for `makerDetails`; the earlier builds' decode is unchanged.
    #[test]
    fn a_v1_row_is_decoded_only_at_its_exact_width() {
        let earlier_position = returns::<Perp::positionsCall>(&mock::position(5_000_000));
        let earlier_maker = returns::<Perp::makerDetailsCall>(&mock::maker(-600, 1_200, 7));
        assert_eq!(
            (earlier_position.len(), earlier_maker.len()),
            (5 * 32, 10 * 32)
        );
        assert!(v1::Perp::positionsCall::abi_decode_returns(&earlier_position).is_ok());
        assert!(v1::Perp::makerDetailsCall::abi_decode_returns(&earlier_maker).is_ok());

        let id = U256::ONE;
        assert!(decode_failed(&decode_exact::<v1::Perp::positionsCall>(
            id,
            &earlier_position,
            "position"
        )));
        assert!(decode_failed(&decode_exact::<v1::Perp::makerDetailsCall>(
            id,
            &earlier_maker,
            "maker details"
        )));
        let mut short = v1_position_words(1, 2, 3, 4);
        short.pop();
        assert!(decode_failed(&decode_exact::<v1::Perp::positionsCall>(
            id, &short, "position"
        )));
        let row =
            decode_exact::<v1::Perp::positionsCall>(id, &v1_position_words(1, 2, 3, 4), "position")
                .unwrap();
        assert_eq!(row.margin, 3);
        let maker = decode_exact::<v1::Perp::makerDetailsCall>(
            id,
            &v1_maker_words(-600, 1_200, 7),
            "maker details",
        )
        .unwrap();
        assert_eq!(maker.liquidity, 7);

        // The other way: the earlier binding cannot read three or seven words.
        assert!(Perp::positionsCall::abi_decode_returns(&v1_position_words(1, 2, 3, 4)).is_err());
        assert!(
            Perp::makerDetailsCall::abi_decode_returns(&v1_maker_words(-600, 1_200, 7)).is_err()
        );
    }

    /// On a `v1.0.0` market every row read, single and batched, refuses an
    /// earlier build's five-word position and ten-word maker answer with
    /// the row's decode error, never a row.
    #[tokio::test]
    async fn v1_row_reads_refuse_an_earlier_build_s_answer() {
        let earlier_position = returns::<Perp::positionsCall>(&mock::position(5_000_000));
        let earlier_maker = returns::<Perp::makerDetailsCall>(&mock::maker(-600, 1_200, 7));
        let (state, rpc) = state().await;
        rpc.immutables(Era::V1);
        rpc.aggregate3(vec![
            mock::ok_row(earlier_position.clone()),
            owner_row(PERP),
        ]);
        assert!(decode_failed(&state.position(U256::ONE).await));
        rpc.raw(&earlier_maker);
        assert!(decode_failed(&state.maker_band(U256::ONE).await));
        rpc.aggregate3(vec![
            mock::ok_row(earlier_position.clone()),
            owner_row(PERP),
        ]);
        let rows = state.positions(&[U256::ONE]).await;
        assert!(decode_failed(&rows[0].row), "{:?}", rows[0].row);
        rpc.aggregate3(vec![mock::ok_row(earlier_maker)]);
        let rows = state.maker_bands(&[U256::ONE]).await;
        assert!(decode_failed(&rows[0].row), "{:?}", rows[0].row);
        rpc.aggregate3(vec![
            mock::ok_row(earlier_position.clone()),
            owner_row(PERP),
        ]);
        assert!(decode_failed(&state.market().get_position(U256::ONE).await));
        assert!(rpc.is_drained());
    }

    /// A hook the chain does not name is refused after the pool key, on
    /// every position read: no row is read and no shape is guessed. A
    /// batch gives every id the typed refusal.
    #[tokio::test]
    async fn every_position_read_refuses_an_unknown_build() {
        let unknown = Address::repeat_byte(0x0e);
        let refused = |err: &PerpCityError| {
            matches!(
                err,
                PerpCityError::Contract(ContractError::UnknownBuild { perp, hook })
                    if *perp == PERP && *hook == unknown
            ) && !err.is_transient()
        };
        let (state, rpc) = state().await;
        let ids = [U256::ONE, U256::from(2u8)];
        for read in 0..5 {
            rpc.call::<Perp::poolKeyCall>(&mock::pool_key_with_hook(30, unknown));
            let errors: Vec<PerpCityError> = match read {
                0 => vec![state.position(U256::ONE).await.unwrap_err()],
                1 => vec![state.maker_band(U256::ONE).await.unwrap_err()],
                2 => state
                    .positions(&ids)
                    .await
                    .into_iter()
                    .map(|o| o.row.unwrap_err())
                    .collect(),
                3 => state
                    .owners(&ids)
                    .await
                    .into_iter()
                    .map(|o| o.row.unwrap_err())
                    .collect(),
                _ => state
                    .maker_bands(&ids)
                    .await
                    .into_iter()
                    .map(|o| o.row.unwrap_err())
                    .collect(),
            };
            assert!(errors.iter().all(refused), "read {read}: {errors:?}");
            assert!(rpc.is_drained(), "read {read}: the pool key only");
        }
        assert_eq!(rpc.selectors(), [Perp::poolKeyCall::SELECTOR; 5]);
    }

    #[tokio::test]
    async fn empty_v1_valuation_batches_require_previews() {
        let (state, rpc) = state().await;
        rpc.immutables(Era::V1);
        for error in [
            state.taker_healths(&[]).await.err().unwrap(),
            state.maker_equities(&[]).await.err().unwrap(),
        ] {
            assert!(matches!(
                error,
                PerpCityError::Contract(ContractError::UnsupportedEra { operation, .. })
                    if operation.contains("preview_positions")
            ));
        }
        assert!(rpc.is_drained());
    }

    #[tokio::test]
    async fn stale_preview_row_keeps_its_selector() {
        let (state, rpc) = state().await;
        rpc.immutables(Era::V1);
        let mut row = mock::failed_row();
        row.returnData = v1::Perp::StaleBeaconIndex::SELECTOR.to_vec().into();
        rpc.aggregate3(vec![row]);
        let rows = state.preview_positions(&[U256::from(1)]).await.unwrap();
        let error = rows[0].row.as_ref().err().unwrap();
        assert!(error.is_transient());
        assert!(
            matches!(error, PerpCityError::Transaction(e) if e.is_revert::<v1::Perp::StaleBeaconIndex>())
        );
    }

    /// The single preview reads a revert as a batch row does: a stale
    /// beacon keeps its selector and is transient, a missing position is
    /// `None`, a fifteen-word answer decodes, and a revert without data
    /// is the batch's empty row.
    #[tokio::test]
    async fn single_preview_matches_the_batch_row() {
        let (state, rpc) = state().await;
        rpc.immutables(Era::V1);
        rpc.reverts(&v1::Perp::StaleBeaconIndex::SELECTOR);
        let error = state.preview_position(U256::ONE).await.err().unwrap();
        assert!(error.is_transient(), "{error}");
        assert!(
            matches!(&error, PerpCityError::Transaction(e) if e.is_revert::<v1::Perp::StaleBeaconIndex>()),
            "{error}"
        );
        rpc.reverts(&v1::Perp::PositionDoesNotExist::SELECTOR);
        assert!(state.preview_position(U256::ONE).await.unwrap().is_none());
        let mut words = vec![0u8; 15 * 32];
        words[31] = 1;
        rpc.raw(&words);
        let preview = state.preview_position(U256::ONE).await.unwrap().unwrap();
        assert!(preview.isMaker);
        rpc.fails("execution reverted");
        let single = state.preview_position(U256::ONE).await.err().unwrap();
        rpc.aggregate3(vec![mock::failed_row()]);
        let rows = state.preview_positions(&[U256::ONE]).await.unwrap();
        for error in [&single, rows[0].row.as_ref().err().unwrap()] {
            assert!(
                matches!(
                    error,
                    PerpCityError::Contract(ContractError::RevertedWithoutSelector { data })
                        if data.is_empty()
                ),
                "{error}"
            );
        }
        assert_eq!(
            rpc.selectors()[2..].to_vec(),
            vec![v1::Perp::previewPositionCall::SELECTOR; 5]
        );
        assert!(rpc.is_drained());
    }

    /// The X64 close depth comes from the two `physicalCap` words at the
    /// handle's block, and the maker total from `fundingState()`.
    #[tokio::test]
    async fn removal_inputs_read_the_v1_market() {
        let (client, rpc) = mock::client();
        let hash = rpc.block(92, TIMESTAMP);
        let state = client.market().state_at(92).await.unwrap();
        rpc.immutables(Era::V1);
        let long = (U256::from(9u8) << 64) + U256::from(3u8);
        let short = U256::from(7u8) << 64;
        rpc.storage(long);
        rpc.storage(short);
        assert_eq!(
            state.physical_close_capacity_x64().await.unwrap(),
            PerSide::new(long, short)
        );
        assert_eq!(rpc.slots(), [U256::from(21u8), U256::from(22u8)]);
        assert_eq!(rpc.storage_blocks(), [BlockId::hash(hash); 2]);
        rpc.call::<v1::Perp::fundingStateCall>(&v1::FundingState {
            baseX96: I256::ZERO,
            inventoryX96: I256::ZERO,
            makerLiquidity: U256::from(42u8),
        });
        assert_eq!(state.maker_liquidity().await.unwrap(), U256::from(42u8));
        assert!(rpc.is_drained());
    }

    /// An empty preview batch checks the era first, as `maker_equities`
    /// and `taker_healths` do; `positions`, `owners` and `maker_bands`
    /// return an empty batch with no request.
    #[tokio::test]
    async fn empty_preview_batch_refuses_an_earlier_build() {
        let (state, rpc) = state().await;
        rpc.immutables(Era::Upgradeable);
        let error = state.preview_positions(&[]).await.err().unwrap();
        assert!(matches!(
            error,
            PerpCityError::Contract(ContractError::UnsupportedEra { operation, .. })
                if operation == "preview_positions"
        ));
        assert!(rpc.is_drained());
    }

    /// The batch reads each build's row: on `v1.0.0` the three-word row,
    /// and a closed taker's deleted row is `None`.
    #[tokio::test]
    async fn positions_read_the_v1_row() {
        let (state, rpc) = state().await;
        rpc.immutables(Era::V1);
        rpc.aggregate3(vec![
            mock::ok_row(v1_position_words(0, -9_000, 100_000_000, 3)),
            owner_row(PERP),
            mock::ok_row(v1_position_words(0, 0, 0, 0)),
            owner_row(Address::ZERO),
            mock::ok_row(v1_position_words(0, 0, 0, 0)),
            mock::failed_row(),
            mock::ok_row(v1_position_words(0, 0, 0, 0)),
            owner_row(PERP),
        ]);
        let rows = state.positions(&[1, 2, 3, 4].map(U256::from)).await;
        let maker = rows[0].row.as_ref().unwrap().unwrap();
        assert_eq!(
            (maker.usd, maker.margin, maker.stored_margin_ratios),
            (UsdcDelta::new(-9_000), UsdcAtoms::new(100_000_000), None)
        );
        assert!(rows[1].row.as_ref().unwrap().is_none(), "the closed taker");
        assert!(rows[2].row.is_err());
        assert_eq!(
            rows[3].row.as_ref().unwrap().unwrap().margin,
            UsdcAtoms::ZERO
        );
        assert_eq!(
            rpc.selectors(),
            [
                Perp::poolKeyCall::SELECTOR,
                v1::Perp::poolIdCall::SELECTOR,
                v1::Perp::positionsCall::SELECTOR,
                v1::Perp::positionOwnerCall::SELECTOR,
                v1::Perp::positionsCall::SELECTOR,
                v1::Perp::positionOwnerCall::SELECTOR,
                v1::Perp::positionsCall::SELECTOR,
                v1::Perp::positionOwnerCall::SELECTOR,
                v1::Perp::positionsCall::SELECTOR,
                v1::Perp::positionOwnerCall::SELECTOR,
            ]
        );
        assert!(rpc.is_drained(), "poolKey, poolId, one multicall");
    }

    /// The earlier builds' batch reads the five-word row, and a v1 answer
    /// fails its id loudly rather than decoding.
    #[tokio::test]
    async fn positions_read_the_earlier_builds_row() {
        for era in [Era::Legacy, Era::Upgradeable] {
            let (state, rpc) = state().await;
            rpc.immutables(era);
            rpc.aggregate3(vec![
                mock::ok_row(mock::returns::<Perp::positionsCall>(&mock::position(5))),
                mock::ok_row(v1_position_words(1, 2, 3, 4)),
            ]);
            let rows = state.positions(&[U256::ONE, U256::from(2u8)]).await;
            let row = rows[0].row.as_ref().unwrap().unwrap();
            assert_eq!(row.margin, UsdcAtoms::new(5));
            assert!(row.stored_margin_ratios.is_some());
            assert!(
                matches!(
                    rows[1].row,
                    Err(PerpCityError::Validation(
                        ValidationError::DecodeFailed { .. }
                    ))
                ),
                "{era:?}: {:?}",
                rows[1].row
            );
            assert!(rpc.is_drained());
        }
    }

    /// A `v1.0.0` market's holders come from `positionOwner`, never
    /// `ownerOf`: the zero address is an inactive id, `None`, and a reverted
    /// row fails alone. The earlier builds' path on the same market would
    /// have failed every row, because `v1.0.0` has no `ownerOf` (an empty
    /// revert, not `TokenDoesNotExist`).
    #[tokio::test]
    async fn owners_on_v1_ask_position_owner() {
        let (state, rpc) = state().await;
        let holder = Address::repeat_byte(0xaa);
        rpc.immutables(Era::V1);
        rpc.aggregate3(vec![
            mock::ok_row(mock::returns::<v1::Perp::positionOwnerCall>(&holder)),
            mock::ok_row(mock::returns::<v1::Perp::positionOwnerCall>(&Address::ZERO)),
            mock::failed_row(),
        ]);
        let ids = [U256::ONE, U256::from(2u8), U256::from(3u8)];
        let rows = state.owners(&ids).await;
        assert_eq!(*rows[0].row.as_ref().unwrap(), Some(holder));
        assert!(rows[1].row.as_ref().unwrap().is_none(), "inactive");
        assert!(rows[2].row.is_err(), "a reverted row is not inactive");
        let selectors = rpc.selectors();
        assert_eq!(
            selectors,
            [
                Perp::poolKeyCall::SELECTOR,
                v1::Perp::poolIdCall::SELECTOR,
                v1::Perp::positionOwnerCall::SELECTOR,
                v1::Perp::positionOwnerCall::SELECTOR,
                v1::Perp::positionOwnerCall::SELECTOR,
            ]
        );
        assert!(!selectors.contains(&Perp::ownerOfCall::SELECTOR));
        assert!(rpc.is_drained());

        // The same market read on the earlier builds' path: v1's answer to
        // `ownerOf` is an empty revert, so every row fails, loudly.
        let (state, rpc) = self::state().await;
        rpc.immutables(Era::Upgradeable);
        rpc.aggregate3(vec![mock::failed_row(), mock::failed_row()]);
        let rows = state.owners(&ids[..2]).await;
        assert!(rows.iter().all(|row| row.row.is_err()));
        assert_eq!(
            rpc.selectors()[2..],
            [Perp::ownerOfCall::SELECTOR, Perp::ownerOfCall::SELECTOR]
        );
    }

    /// A `v1.0.0` band is the first three of its seven words, singly and in
    /// a batch; an empty band is `None`; the earlier builds' shape on a v1
    /// answer fails loudly.
    #[tokio::test]
    async fn maker_bands_read_the_v1_maker() {
        let band = Some(MakerBand::new(
            TickRange::new(38_400, 39_840).unwrap(),
            LUnits::new(100_000_000),
        ));
        let (state, rpc) = state().await;
        rpc.immutables(Era::V1);
        rpc.raw(&v1_maker_words(38_400, 39_840, 100_000_000));
        assert_eq!(state.maker_band(U256::ONE).await.unwrap(), band);
        rpc.aggregate3(vec![
            mock::ok_row(v1_maker_words(38_400, 39_840, 100_000_000)),
            mock::ok_row(v1_maker_words(0, 0, 0)),
        ]);
        let rows = state.maker_bands(&[U256::ONE, U256::from(2u8)]).await;
        assert_eq!(*rows[0].row.as_ref().unwrap(), band);
        assert!(rows[1].row.as_ref().unwrap().is_none(), "the taker");
        assert!(rpc.is_drained());

        let (state, rpc) = self::state().await;
        rpc.immutables(Era::Upgradeable);
        rpc.aggregate3(vec![mock::ok_row(v1_maker_words(38_400, 39_840, 1))]);
        let rows = state.maker_bands(&[U256::ONE]).await;
        assert!(
            matches!(
                rows[0].row,
                Err(PerpCityError::Validation(
                    ValidationError::DecodeFailed { .. }
                ))
            ),
            "{:?}",
            rows[0].row
        );
    }

    /// An era read the endpoint fails gives every id of the batch the
    /// failure, kept transient; no id is dropped.
    #[tokio::test]
    async fn a_failed_era_read_fails_every_id_of_the_batch() {
        let (state, rpc) = state().await;
        rpc.fails("connection reset");
        let rows = state.owners(&[U256::ONE, U256::from(2u8)]).await;
        assert_eq!(rows.len(), 2);
        for row in &rows {
            let Err(err) = &row.row else {
                panic!("every id fails");
            };
            assert!(err.is_transient(), "{err}");
        }
        assert!(rpc.is_drained());
    }

    // ── The batched row read ──────────────────────────────────────────

    /// Every id gets its row from one multicall, in input order: a present
    /// position, an empty row, and a reverted row that fails alone and
    /// deterministically.
    #[tokio::test]
    async fn positions_read_every_row_in_one_multicall() {
        let (state, rpc) = state().await;
        let pos_ids = [U256::from(11u8), U256::from(22u8), U256::from(33u8)];
        rpc.immutables(Era::Legacy);
        rpc.aggregate3(vec![
            mock::ok_row(returns::<Perp::positionsCall>(&mock::position(5_000_000))),
            mock::ok_row(returns::<Perp::positionsCall>(&mock::position(0))),
            mock::failed_row(),
        ]);

        let rows = state.positions(&pos_ids).await;
        assert_eq!(rows.iter().map(|r| r.pos_id).collect::<Vec<_>>(), pos_ids);
        assert_eq!(
            rows[0].row.as_ref().unwrap().as_ref().unwrap().margin,
            UsdcAtoms::new(5_000_000)
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

    /// Owners come back in input order from one multicall: a holder, a
    /// burned id as `None`, and a revert that is not the token's own
    /// failing alone rather than reading as burned.
    #[tokio::test]
    async fn owners_tell_a_burned_id_from_a_failed_read() {
        let (state, rpc) = state().await;
        let holder = Address::repeat_byte(0xaa);
        rpc.immutables(Era::Legacy);
        rpc.aggregate3(vec![
            mock::ok_row(returns::<Perp::ownerOfCall>(&holder)),
            IMulticall3::Result {
                success: false,
                returnData: Perp::TokenDoesNotExist::SELECTOR.into(),
            },
            mock::failed_row(),
        ]);

        let rows = state
            .owners(&[U256::ONE, U256::from(2u8), U256::from(3u8)])
            .await;
        assert_eq!(*rows[0].row.as_ref().unwrap(), Some(holder));
        assert!(rows[1].row.as_ref().unwrap().is_none());
        assert!(rows[2].row.is_err(), "an out-of-gas row is not a burn");
        assert!(rpc.is_drained(), "one multicall");
    }

    /// A multicall the replica cannot serve fails every id of the chunk
    /// with the typed, transient answer; no id is dropped.
    #[tokio::test]
    async fn a_failed_row_multicall_fails_every_id_of_the_chunk_naming_the_block() {
        let (state, rpc) = state().await;
        rpc.immutables(Era::Legacy);
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
        rpc.immutables(Era::Legacy);
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
        rpc.immutables(Era::Legacy);
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
                close_capacity: None,
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

    /// The two immutables the first snapshot reads: the pool key, then the pool id.
    fn immutables_answers(rpc: &Rpc) {
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key(SPACING));
        rpc.call::<Perp::POOL_IDCall>(&POOL_ID);
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
            rpc.call::<Perp::poolKeyCall>(&mock::pool_key(spacing));
            rpc.call::<Perp::POOL_IDCall>(&POOL_ID);

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
