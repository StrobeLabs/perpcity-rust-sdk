//! The maker-equity batch: block-pinned settle previews for a set of
//! position ids.
//!
//! The pure settlement math lives in [`crate::math::maker_equity`]; this
//! module is its composition over one [`StateAt`]: the market-wide
//! multicall, then per chunk of ids the maker rows, the PoolManager
//! `extsload` over the V4 fee-growth slots, and the `eth_getProof` /
//! `eth_getStorageAt` reads of the Perp tick-funding slots. The three
//! storage reads exist because the deployed contracts expose no settle
//! preview; the next era's `previewPosition` replaces this file wholesale,
//! so they stay crate-private.

use std::collections::{BTreeMap, BTreeSet};
use std::future::IntoFuture;
use std::sync::Arc;

use alloy::primitives::{Address, B256, I256, U256};
use alloy::providers::Provider;
use alloy::sol_types::SolCall;
use alloy::transports::{TransportError, TransportErrorKind};
use futures_util::stream::{self, StreamExt};

use crate::contracts::{IPoolManagerState, Maker, Perp, Position};
use crate::convert::unpack_balance_delta;
use crate::errors::{ContractError, PerpCityError, Result, ValidationError};
use crate::math::maker_equity::{
    AccrualInputs, AccruedMakerSnapshot, MakerEquityBreakdown, MakerMarketSnapshot, MakerState,
    TickFunding, fee_growth_inside1,
};
use crate::math::pricing::PricePair;
use crate::math::tick::get_sqrt_ratio_at_tick;
use crate::storage::{
    perp_tick_funding_slots, v4_fee_growth_global1_slot, v4_position_fee_growth_inside1_slot,
    v4_tick_fee_growth_outside1_slot,
};

use super::market::MarketReader;
use super::state::{
    CHUNK_READ_CONCURRENCY, ChunkReadFailure, MAX_ROW_BATCH, PerpViews, RowOutcome, StateAt,
    decode_row, ema_window_secs,
};
use super::{i24_to_i32, u24_to_u32};

/// Concurrency bound for the `eth_getStorageAt` fallback when the endpoint
/// does not serve `eth_getProof`.
const TICK_READ_CONCURRENCY: usize = 16;

/// Outcome of one tick's funding read: the two decoded words, or the shared
/// transport error that failed every position referencing the tick.
type TickFundingRead = std::result::Result<TickFunding, Arc<TransportError>>;

/// The batch outcome for one requested position id: every id passed to
/// [`StateAt::maker_equities`] comes back as exactly one of these, in
/// input order.
#[derive(Debug)]
pub struct MakerEquityOutcome {
    /// The requested position id.
    pub pos_id: U256,
    /// What the read produced for it.
    pub kind: MakerEquityKind,
}

/// What a maker-equity batch read produced for one position id.
#[derive(Debug)]
pub enum MakerEquityKind {
    /// An open maker position, with its settle preview.
    Computed(MakerEquityBreakdown),
    /// Zero liquidity at the pinned block: a taker, a burned position, or
    /// a never-minted id. Nothing to compute — not an error.
    NotAMaker,
    /// This position's reads, decoding, or settle math failed; the rest of
    /// the batch is unaffected. Retry exactly when
    /// [`PerpCityError::is_transient`] says so.
    Failed(PerpCityError),
}

/// How many of a batch's ids resolved to each [`MakerEquityKind`].
#[derive(Debug, Default, PartialEq, Eq)]
struct Tally {
    computed: usize,
    not_a_maker: usize,
    failed: usize,
}

impl Tally {
    fn of(kinds: &[MakerEquityKind]) -> Self {
        let mut tally = Self::default();
        for kind in kinds {
            match kind {
                MakerEquityKind::Computed(_) => tally.computed += 1,
                MakerEquityKind::NotAMaker => tally.not_a_maker += 1,
                MakerEquityKind::Failed(_) => tally.failed += 1,
            }
        }
        tally
    }
}

/// A maker position that survived the row read and awaits the slot reads.
struct PendingMaker {
    /// Index into the chunk's ids, so per-position results merge back
    /// into input order without positional bookkeeping.
    input_index: usize,
    pos_id: U256,
    position: Position,
    details: Maker,
    tick_lower: i32,
    tick_upper: i32,
}

impl PendingMaker {
    /// The rows as the slot reads need them. Ticks are validated against
    /// the Uniswap domain here so no slot is derived from a tick the
    /// downstream math would reject.
    fn new(input_index: usize, pos_id: U256, position: Position, details: Maker) -> Result<Self> {
        let tick_lower = i24_to_i32(details.tickLower);
        let tick_upper = i24_to_i32(details.tickUpper);
        get_sqrt_ratio_at_tick(tick_lower)?;
        get_sqrt_ratio_at_tick(tick_upper)?;
        Ok(Self {
            input_index,
            pos_id,
            position,
            details,
            tick_lower,
            tick_upper,
        })
    }
}

/// Layout of the single PoolManager `extsload` batch over the V4 fee-growth
/// state for a set of maker positions: word 0 is the pool's
/// `feeGrowthGlobal1X128`, followed by one `feeGrowthOutside1X128` word per
/// DISTINCT band tick (positions in a maker ladder share boundaries, so a
/// shared tick is read once), then one `feeGrowthInside1LastX128` word per
/// position.
struct FeeGrowthLayout {
    /// Word index of each distinct tick's outside-growth slot.
    tick_word: BTreeMap<i32, usize>,
    /// Word index of the first per-position inside-growth slot.
    first_inside_word: usize,
    /// Total words the extsload response must carry.
    word_count: usize,
}

impl FeeGrowthLayout {
    /// Build the layout and the slot vector it indexes into. The vector is
    /// returned by value (the extsload call consumes it) rather than stored
    /// and cloned.
    fn new(pool_id: B256, perp: Address, pending: &[PendingMaker]) -> (Self, Vec<B256>) {
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
        (
            Self {
                tick_word,
                first_inside_word,
                word_count: slots.len(),
            },
            slots,
        )
    }
}

/// The V4 fee-growth words of a set of maker positions at one block, read
/// in one `extsload` laid out by [`FeeGrowthLayout`].
struct FeeGrowth {
    layout: FeeGrowthLayout,
    words: Vec<B256>,
}

impl FeeGrowth {
    /// The pool's `feeGrowthGlobal1X128`: word 0 by construction.
    fn global(&self) -> U256 {
        U256::from_be_bytes(self.words[0].0)
    }

    /// `feeGrowthOutside1X128` of `tick`, which must have been a band
    /// boundary of the positions the read was built from.
    fn outside(&self, tick: i32) -> Result<U256> {
        self.layout
            .tick_word
            .get(&tick)
            .map(|&word| U256::from_be_bytes(self.words[word].0))
            .ok_or_else(|| {
                ContractError::StorageReadFailed {
                    context: format!("tick {tick} missing from fee-growth layout"),
                    source: None,
                }
                .into()
            })
    }

    /// `feeGrowthInside1LastX128` of the `position`-th pending position.
    fn inside_last(&self, position: usize) -> U256 {
        U256::from_be_bytes(self.words[self.layout.first_inside_word + position].0)
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
            source: Some(Arc::clone(e)),
        }
        .into()),
        None => Err(ContractError::StorageReadFailed {
            context: format!("tick {tick} funding missing from batch"),
            source: None,
        }
        .into()),
    }
}

/// The JSON-RPC "method not found" code — the one standardized signal
/// that a method is unavailable.
const METHOD_NOT_FOUND: i64 = -32601;

/// Whether a transport error is the endpoint's definitive statement that
/// the RPC method does not exist there (JSON-RPC `-32601`), as opposed to
/// the call failing. Only this answer is worth remembering: any other
/// failure of `eth_getProof` still falls back to `eth_getStorageAt` for
/// the current read, but is not held against later reads — providers
/// phrase transient and permanent failures too much alike to latch a
/// permanent, client-wide policy on message text.
fn method_unsupported(e: &TransportError) -> bool {
    e.as_error_resp()
        .is_some_and(|resp| resp.code == METHOD_NOT_FOUND)
}

impl MarketReader {
    /// [`StateAt::maker_equities`] at the lagged snapshot block. An empty
    /// input resolves no block.
    pub async fn get_maker_equities(&self, pos_ids: &[U256]) -> Result<Vec<MakerEquityOutcome>> {
        if pos_ids.is_empty() {
            return Ok(Vec::new());
        }
        self.state().await?.maker_equities(pos_ids).await
    }

    /// [`StateAt::maker_equities_at_mark`] at the lagged snapshot block.
    pub async fn get_maker_equities_at_mark(
        &self,
        pos_ids: &[U256],
        mark_price_x96: U256,
    ) -> Result<Vec<MakerEquityOutcome>> {
        if pos_ids.is_empty() {
            return Ok(Vec::new());
        }
        self.state()
            .await?
            .maker_equities_at_mark(pos_ids, mark_price_x96)
            .await
    }
}

impl StateAt {
    /// The settle-preview equity of each maker position in `pos_ids`, every
    /// read at this block.
    ///
    /// Returns exactly one [`MakerEquityOutcome`] per input id, in input
    /// order: [`Computed`](MakerEquityKind::Computed) for an open maker,
    /// [`NotAMaker`](MakerEquityKind::NotAMaker) for zero-liquidity ids
    /// (takers, burned, never minted), and
    /// [`Failed`](MakerEquityKind::Failed) when that one position's reads,
    /// decoding, or settle math failed — the rest of the batch is
    /// unaffected. A read shared by one chunk failing marks the ids it was
    /// serving `Failed` with the shared cause (its row multicall: every id
    /// of the chunk; its fee-growth `extsload`: the chunk's open makers)
    /// and leaves the other chunks intact. Only the market-wide read fails
    /// the whole call.
    ///
    /// A `Failed` outcome is worth retrying exactly when its error's
    /// [`PerpCityError::is_transient`] is true (a lagging replica or a
    /// dropped storage read); decode and settle-math failures are
    /// deterministic and will not clear on retry.
    ///
    /// Reads are batched: one multicall for the market-wide state and the
    /// beacon's index once; then per chunk of at most [`MAX_ROW_BATCH`]
    /// ids, one row multicall ([`Self::positions`]'s shape, with the maker
    /// row beside each position row), one PoolManager `extsload` for the
    /// V4 fee-growth slots (distinct band ticks read once), and one
    /// `eth_getProof` for the distinct Perp tick-funding slots (with an
    /// `eth_getStorageAt` fallback on endpoints without `eth_getProof`).
    ///
    /// The mark that prices `valPnl` and the accrual replay is the
    /// contract's own, the value [`Self::mark`] reads: the deployed fair
    /// price ([`crate::math::pricing::fair_price_x96`]) of this block's
    /// `poolState().ammPrice`, beacon index, and EMAs advanced to the block
    /// timestamp — exactly what `PerpLogic.accrue` sets as `markPrice`
    /// when it touches the market, so
    /// [`MakerEquityBreakdown::is_liquidatable`] agrees with the chain's
    /// health check. For what-if pricing at a caller-chosen mark, use
    /// [`Self::maker_equities_at_mark`].
    pub async fn maker_equities(&self, pos_ids: &[U256]) -> Result<Vec<MakerEquityOutcome>> {
        self.maker_equities_inner(pos_ids, None).await
    }

    /// [`Self::maker_equities`] priced at a caller-supplied mark (exact
    /// X96) instead of this block's fair price — what-if pricing for
    /// stress marks or off-snapshot scenarios. All chain state is still
    /// read at this block; only the pricing input changes.
    pub async fn maker_equities_at_mark(
        &self,
        pos_ids: &[U256],
        mark_price_x96: U256,
    ) -> Result<Vec<MakerEquityOutcome>> {
        if mark_price_x96.is_zero() {
            return Err(ValidationError::InvalidPrice {
                reason: "mark_price_x96 must be non-zero".into(),
            }
            .into());
        }
        self.maker_equities_inner(pos_ids, Some(mark_price_x96))
            .await
    }

    async fn maker_equities_inner(
        &self,
        pos_ids: &[U256],
        mark_override_x96: Option<U256>,
    ) -> Result<Vec<MakerEquityOutcome>> {
        if pos_ids.is_empty() {
            return Ok(Vec::new());
        }
        let pool_id = self.market().immutables().await?.pool_id;
        let market = self.maker_market(mark_override_x96).await?;

        // Chunks are disjoint id ranges at the one block, so they read
        // concurrently (bounded); `buffered` yields them in input order,
        // which the zip below relies on. Collected into a Vec first so the
        // returned future's Send bound is provable from the concrete
        // future type, not the borrowing iterator adapter.
        let market = &market;
        let chunk_reads: Vec<_> = pos_ids
            .chunks(MAX_ROW_BATCH)
            .map(|chunk| self.chunk_equities(market, pool_id, chunk))
            .collect();
        let kinds: Vec<MakerEquityKind> = stream::iter(chunk_reads)
            .buffered(CHUNK_READ_CONCURRENCY)
            .concat()
            .await;

        // The batch degrades per position instead of failing, so this line is
        // the only place the shape of what came back is visible: a sweep that
        // is 40% `Failed` otherwise reads exactly like a clean one. A failure
        // is worth seeing without turning debug on, so the outcome picks the
        // level; the per-position causes are already logged above.
        let Tally {
            computed,
            not_a_maker,
            failed,
        } = Tally::of(&kinds);
        let block = self.block().number;
        if failed > 0 {
            tracing::warn!(
                count = pos_ids.len(),
                computed,
                not_a_maker,
                failed,
                block,
                "maker equities read, some positions failed"
            );
        } else {
            tracing::debug!(
                count = pos_ids.len(),
                computed,
                not_a_maker,
                failed,
                block,
                "maker equities read"
            );
        }
        Ok(pos_ids
            .iter()
            .zip(kinds)
            .map(|(&pos_id, kind)| MakerEquityOutcome { pos_id, kind })
            .collect())
    }

    /// The market-wide settle inputs at this block, accrued to its
    /// timestamp: one multicall over the `Perp`, then the beacon's index,
    /// both pinned here.
    ///
    /// The accrual replay always runs at the contract's mark for the block
    /// — `fairPrice(ammPrice, index, emas)` with the stored EMAs advanced to
    /// the block timestamp, as `PerpLogic.accrue` computes it;
    /// `mark_override_x96` then reprices the accrued snapshot for what-if
    /// pricing. A failed beacon read fails the whole call, like any other
    /// market-wide read.
    async fn maker_market(&self, mark_override_x96: Option<U256>) -> Result<AccruedMakerSnapshot> {
        let chain = self.market().chain();
        let perp = Perp::new(self.market().perp(), chain.provider());
        let (modules, pool_state, emas, rates, ema_window, cumls, capacity, oi) = chain
            .multicall_at(self.id())
            .add(perp.modules())
            .add(perp.poolState())
            .add(perp.emas())
            .add(perp.rates())
            .add(perp.EMA_WINDOW())
            .add(perp.cumulatives())
            .add(perp.capacity())
            .add(perp.openInterest())
            .aggregate()
            .await
            .map_err(|e| self.multicall_read_error(e))?;
        let views = PerpViews {
            modules,
            pool_state,
            stored_emas: PricePair {
                amm: emas.ammPrice,
                index: emas.index,
            },
            last_touch: rates.lastTouch.to::<u64>(),
            ema_window: ema_window_secs(ema_window)?,
        };
        let mark = self.mark_from(&views).await?;
        let block = self.block();

        let market = MakerMarketSnapshot {
            block,
            funding_x96: cumls.fundingX96,
            funding_div_sqrt_p_x96: cumls.fundingDivSqrtPX96,
            long_util_earnings_x96: cumls.longUtilEarningsX96,
            short_util_earnings_x96: cumls.shortUtilEarningsX96,
            tick: i24_to_i32(views.pool_state.tick),
            sqrt_price_x96: views.pool_state.sqrtPrice.to::<U256>(),
            mark_price_x96: mark.fair_price_x96(),
        }
        .accrued(&AccrualInputs {
            funding_per_day_wad: i128::try_from(rates.fundingPerDay)
                .expect("int88 always fits i128"),
            long_util_fee_per_day_wad: rates.longUtilFeePerDay,
            short_util_fee_per_day_wad: rates.shortUtilFeePerDay,
            last_touch: views.last_touch,
            accrue_to: block.timestamp,
            oi_long_atoms: oi.long,
            oi_short_atoms: oi.short,
            cap_long_atoms: capacity.long,
            cap_short_atoms: capacity.short,
        })?;
        // The what-if mark is applied AFTER the replay: the elapsed accrual
        // happened at the chain's mark, and only the pricing legs are the
        // caller's to override.
        Ok(match mark_override_x96 {
            Some(mark_price_x96) => market.with_mark(mark_price_x96),
            None => market,
        })
    }

    /// One chunk of the batch: its maker rows, then the slot reads and
    /// math for the makers among them, producing one [`MakerEquityKind`]
    /// per chunk id in input order.
    async fn chunk_equities(
        &self,
        market: &AccruedMakerSnapshot,
        pool_id: B256,
        pos_ids: &[U256],
    ) -> Vec<MakerEquityKind> {
        // Ids that produce neither a pending maker nor a failure were
        // zero-liquidity rows.
        let mut kinds: Vec<MakerEquityKind> =
            pos_ids.iter().map(|_| MakerEquityKind::NotAMaker).collect();
        let mut pending = Vec::new();
        for (input_index, outcome) in self.maker_rows(pos_ids).await.into_iter().enumerate() {
            match outcome.row {
                Ok(None) => {}
                Ok(Some((position, details))) => {
                    match PendingMaker::new(input_index, outcome.pos_id, position, details) {
                        Ok(maker) => pending.push(maker),
                        Err(e) => {
                            tracing::debug!(
                                pos_id = %outcome.pos_id, error = %e,
                                "maker equity: band rejected"
                            );
                            kinds[input_index] = MakerEquityKind::Failed(e);
                        }
                    }
                }
                Err(e) => kinds[input_index] = MakerEquityKind::Failed(e),
            }
        }
        let equities = self.pending_equities(market, pool_id, &pending).await;
        for (maker, equity) in pending.iter().zip(equities) {
            kinds[maker.input_index] = match equity {
                Ok(breakdown) => MakerEquityKind::Computed(breakdown),
                Err(e) => MakerEquityKind::Failed(e),
            };
        }
        kinds
    }

    /// The position and maker rows of `pos_ids`, as [`Self::positions`]
    /// reads rows: `None` for an id holding no liquidity at this block (a
    /// taker, a burned position, a never-minted id).
    pub(super) async fn maker_rows(&self, pos_ids: &[U256]) -> Vec<RowOutcome<(Position, Maker)>> {
        self.rows(
            pos_ids,
            |pos_id| {
                [
                    Perp::positionsCall { posId: pos_id }.abi_encode(),
                    Perp::makerDetailsCall { posId: pos_id }.abi_encode(),
                ]
            },
            |pos_id, [position_row, details_row]| {
                let position = decode_row::<Perp::positionsCall>(pos_id, position_row, "position")?;
                let details =
                    decode_row::<Perp::makerDetailsCall>(pos_id, details_row, "makerDetails")?;
                Ok((details.liquidity != 0).then_some((position, details)))
            },
        )
        .await
    }

    /// Slot reads and math for the makers that survived the row read: the
    /// fee-growth `extsload` and the tick-funding read, both at this block
    /// and concurrent (the tick set comes from the rows, not from the
    /// fee-growth words), then the settle math per position. A failed
    /// fee-growth read fails every maker here with the shared cause; a
    /// failed tick read fails the makers referencing that tick.
    async fn pending_equities(
        &self,
        market: &AccruedMakerSnapshot,
        pool_id: B256,
        pending: &[PendingMaker],
    ) -> Vec<Result<MakerEquityBreakdown>> {
        if pending.is_empty() {
            return Vec::new();
        }
        // Positions share band boundaries (a maker ladder reuses each inner
        // tick twice), so read each distinct tick's two funding words once.
        let ticks: BTreeSet<i32> = pending
            .iter()
            .flat_map(|maker| [maker.tick_lower, maker.tick_upper])
            .collect();
        let (fee_growth, tick_funding) =
            tokio::join!(self.fee_growth(pool_id, pending), self.tick_funding(&ticks));
        let fee_growth = match fee_growth {
            Ok(fee_growth) => fee_growth,
            Err(e) => {
                tracing::debug!(
                    makers = pending.len(),
                    error = %e,
                    "maker equity: fee-growth read failed"
                );
                let failure = ChunkReadFailure::new("maker equity fee-growth read", e);
                return pending.iter().map(|_| Err(failure.error())).collect();
            }
        };
        let fg1_global = fee_growth.global();
        let current_tick = market.snapshot().tick;

        pending
            .iter()
            .enumerate()
            .map(|(i, maker)| {
                let tick_lower_funding = tick_funding_for(&tick_funding, maker.tick_lower)?;
                let tick_upper_funding = tick_funding_for(&tick_funding, maker.tick_upper)?;
                let fg1_out_lower = fee_growth.outside(maker.tick_lower)?;
                let fg1_out_upper = fee_growth.outside(maker.tick_upper)?;
                let (delta_amount0, delta_amount1) = unpack_balance_delta(maker.position.delta);
                let state = MakerState {
                    margin_atoms: maker.position.margin,
                    liq_margin_ratio_e6: u24_to_u32(maker.position.liqMarginRatio),
                    delta_amount0,
                    delta_amount1,
                    last_cuml_funding_x96: maker.position.lastCumlFundingX96,
                    tick_lower: maker.tick_lower,
                    tick_upper: maker.tick_upper,
                    liquidity: maker.details.liquidity,
                    last_long_util_earnings_x96: maker.details.lastLongUtilEarningsX96,
                    last_short_util_earnings_x96: maker.details.lastShortUtilEarningsX96,
                    cap_long_atoms: maker.details.capacity.long,
                    cap_short_atoms: maker.details.capacity.short,
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
                        current_tick,
                    ),
                    fee_growth_inside1_last_x128: fee_growth.inside_last(i),
                };
                market.maker_equity(&state).map_err(|e| {
                    tracing::debug!(
                        pos_id = %maker.pos_id, error = %e,
                        "maker equity: settle math failed"
                    );
                    e.into()
                })
            })
            .collect()
    }

    /// The V4 fee-growth words of `pending`'s liquidity positions at this
    /// block: the pool's global word, each distinct band tick's outside
    /// word, and each position's inside-last word, in one PoolManager
    /// `extsload`.
    async fn fee_growth(&self, pool_id: B256, pending: &[PendingMaker]) -> Result<FeeGrowth> {
        let chain = self.market().chain();
        let (layout, slots) = FeeGrowthLayout::new(pool_id, self.market().perp(), pending);
        let words = IPoolManagerState::new(chain.deployments().pool_manager, chain.provider())
            .extsload_1(slots)
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        if words.len() != layout.word_count {
            return Err(ContractError::StorageReadFailed {
                context: format!(
                    "maker equity extsload returned {} words, expected {}",
                    words.len(),
                    layout.word_count
                ),
                source: None,
            }
            .into());
        }
        Ok(FeeGrowth { layout, words })
    }

    /// Each distinct tick's two funding words from the Perp contract, at
    /// this block.
    ///
    /// Primary path: one `eth_getProof` request over all slots —
    /// `storageProof[i].value` carries the storage words, the request takes
    /// a block id so the batch stays pinned, and Arbitrum Nitro serves it.
    /// Any `eth_getProof` failure falls back to concurrent
    /// `eth_getStorageAt` with bounded concurrency; an endpoint that
    /// answers "method not found" is remembered on the transport so the
    /// probe is not repeated. On both paths a failed (or missing) tick read
    /// degrades only the positions referencing that tick.
    pub(super) async fn tick_funding(
        &self,
        ticks: &BTreeSet<i32>,
    ) -> BTreeMap<i32, TickFundingRead> {
        let chain = self.market().chain();
        let perp_addr = self.market().perp();
        if chain.transport().supports_get_proof() {
            let keys: Vec<B256> = ticks
                .iter()
                .flat_map(|&tick| perp_tick_funding_slots(tick).map(B256::from))
                .collect();
            match chain
                .provider()
                .get_proof(perp_addr, keys)
                .block_id(self.id())
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
                        let [slot_opp, slot_div] = perp_tick_funding_slots(tick);
                        let word = |slot: U256| values.get(&B256::from(slot)).copied();
                        let (Some(opp), Some(div_sqrt_p_opp)) = (word(slot_opp), word(slot_div))
                        else {
                            // Degrade per tick, exactly like the fallback
                            // path: only the positions referencing this
                            // tick fail, and retryably (a replica may have
                            // dropped part of the proof).
                            tracing::debug!(
                                tick,
                                "eth_getProof response missing tick storage slots"
                            );
                            funding.insert(
                                tick,
                                Err(Arc::new(TransportErrorKind::custom_str(
                                    "eth_getProof response missing the tick's storage slots",
                                ))),
                            );
                            continue;
                        };
                        funding.insert(
                            tick,
                            Ok(TickFunding {
                                cuml_funding_opp_x96: I256::from_raw(opp),
                                cuml_funding_div_sqrt_p_opp_x96: I256::from_raw(div_sqrt_p_opp),
                            }),
                        );
                    }
                    return funding;
                }
                Err(e) if method_unsupported(&e) => {
                    tracing::debug!(
                        error = %e,
                        "eth_getProof unsupported by endpoint; \
                         falling back to eth_getStorageAt"
                    );
                    chain.transport().note_get_proof_unsupported();
                }
                // Any other failure (rate limit, timeout, a replica hiccup)
                // falls back for this read only — the per-tick reads give
                // the batch a second chance, and if they fail too the
                // failure surfaces per position from the fallback below.
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        "eth_getProof failed; falling back to eth_getStorageAt for this read"
                    );
                }
            }
        }

        // Fallback: all reads are pinned to the same block and independent,
        // so every tick's slot pair runs concurrently (bounded so a large
        // ladder cannot flood the endpoint).
        // Collected into a Vec first so the returned future's Send bound is
        // provable from the concrete future type, not the borrowing
        // iterator adapter.
        let block_id = self.id();
        let tick_reads: Vec<_> = ticks
            .iter()
            .map(|&tick| {
                let [slot_opp, slot_div] = perp_tick_funding_slots(tick);
                let read = |slot: U256| {
                    chain
                        .provider()
                        .get_storage_at(perp_addr, slot)
                        .block_id(block_id)
                        .into_future()
                };
                async move {
                    let funding = tokio::try_join!(read(slot_opp), read(slot_div)).map(
                        |(opp, div_sqrt_p_opp)| TickFunding {
                            cuml_funding_opp_x96: I256::from_raw(opp),
                            cuml_funding_div_sqrt_p_opp_x96: I256::from_raw(div_sqrt_p_opp),
                        },
                    );
                    (tick, funding)
                }
            })
            .collect();
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
                    tracing::debug!(tick, error = %e, "maker equity: tick funding read failed");
                    funding.insert(tick, Err(Arc::new(e)));
                }
            }
        }
        funding
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, I256, Uint};

    use super::*;
    use crate::client::mock::{self, PERP, Rpc, returns, x96};
    use crate::contracts::{Cumulatives, IBeacon, IMulticall3, Rates};

    const TIMESTAMP: u64 = 1_700_000_000;
    /// The pool's id, as `POOL_ID()` reports it.
    const POOL_ID: B256 = B256::repeat_byte(0x99);

    /// A handle pinned to block 92 by name, over a mocked reader.
    async fn state() -> (StateAt, Rpc) {
        let (client, rpc) = mock::client();
        rpc.block(92, TIMESTAMP);
        let state = client.market().state_at(92).await.unwrap();
        (state, rpc)
    }

    fn pending_maker(input_index: usize, pos_id: u8, lower: i32, upper: i32) -> PendingMaker {
        PendingMaker::new(
            input_index,
            U256::from(pos_id),
            mock::position(0),
            mock::maker(lower, upper, 1),
        )
        .unwrap()
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
        let (layout, slots) = FeeGrowthLayout::new(pool_id, perp, &pending);

        // 1 global + 4 distinct ticks + 3 positions.
        assert_eq!(layout.word_count, 8);
        assert_eq!(slots.len(), 8);
        assert_eq!(slots[0], B256::from(v4_fee_growth_global1_slot(pool_id)));
        for tick in [-60, 60, 120, 180] {
            assert_eq!(
                slots[layout.tick_word[&tick]],
                B256::from(v4_tick_fee_growth_outside1_slot(pool_id, tick)),
                "outside slot for tick {tick}"
            );
        }
        for (i, maker) in pending.iter().enumerate() {
            assert_eq!(
                slots[layout.first_inside_word + i],
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

        // Word extraction follows the same indices; a tick outside the
        // build set is a typed failure instead of a panic.
        let words: Vec<B256> = (0u8..8).map(B256::repeat_byte).collect();
        let tick_60 = layout.tick_word[&60];
        let inside_2 = layout.first_inside_word + 2;
        let fee_growth = FeeGrowth { layout, words };
        assert_eq!(
            fee_growth.global(),
            U256::from_be_bytes(fee_growth.words[0].0)
        );
        assert_eq!(
            fee_growth.outside(60).unwrap(),
            U256::from_be_bytes(fee_growth.words[tick_60].0)
        );
        assert!(matches!(
            fee_growth.outside(90).unwrap_err(),
            PerpCityError::Contract(ContractError::StorageReadFailed { source: None, .. })
        ));
        assert_eq!(
            fee_growth.inside_last(2),
            U256::from_be_bytes(fee_growth.words[inside_2].0)
        );
    }

    /// A band the contract could never store fails before any slot is
    /// derived from it.
    #[test]
    fn a_band_outside_the_tick_domain_is_rejected_before_the_slot_reads() {
        let Err(err) = PendingMaker::new(
            0,
            U256::ONE,
            mock::position(0),
            mock::maker(-1_000_000, 60, 1),
        ) else {
            panic!("a tick below MIN_TICK must be rejected");
        };
        assert!(matches!(
            err,
            PerpCityError::Validation(ValidationError::InvalidTickRange { .. })
        ));
    }

    /// A failed tick read must fail exactly the positions referencing that
    /// tick: both neighbours of a shared failed boundary degrade, while a
    /// position whose band avoids it computes normally.
    #[test]
    fn failed_tick_read_degrades_only_referencing_positions() {
        let shared_error = Arc::new(TransportErrorKind::custom_str("replica dropped the read"));
        let funding: BTreeMap<i32, TickFundingRead> = BTreeMap::from([
            (-60, Ok(TickFunding::default())),
            (60, Err(Arc::clone(&shared_error))),
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

    /// The aggregate log line is the only caller-visible summary of a batch
    /// that degrades per position, so the tally must attribute every id —
    /// a miscount is a sweep reported cleaner than it was.
    #[test]
    fn tally_counts_every_outcome_kind() {
        let failed = || {
            MakerEquityKind::Failed(PerpCityError::Contract(ContractError::MulticallFailed {
                reason: "row reverted".into(),
            }))
        };
        let kinds = vec![
            MakerEquityKind::Computed(MakerEquityBreakdown::default()),
            MakerEquityKind::NotAMaker,
            failed(),
            MakerEquityKind::Computed(MakerEquityBreakdown::default()),
            failed(),
            failed(),
        ];

        let tally = Tally::of(&kinds);
        assert_eq!(
            tally,
            Tally {
                computed: 2,
                not_a_maker: 1,
                failed: 3,
            }
        );
        assert_eq!(
            tally.computed + tally.not_a_maker + tally.failed,
            kinds.len(),
            "every id must be counted exactly once"
        );

        assert_eq!(Tally::of(&[]), Tally::default());
    }

    /// Only the standardized "method not found" code may latch the
    /// client-wide `eth_getStorageAt` policy: a rate limit or a replica
    /// hiccup whose message merely resembles it must not permanently
    /// switch every later read off `eth_getProof`.
    #[test]
    fn only_method_not_found_latches_the_fallback() {
        let resp = |code: i64, message: &'static str| {
            TransportError::ErrorResp(alloy::rpc::json_rpc::ErrorPayload {
                code,
                message: message.into(),
                data: None,
            })
        };

        assert!(method_unsupported(&resp(
            METHOD_NOT_FOUND,
            "the method eth_getProof does not exist/is not available"
        )));
        assert!(!method_unsupported(&resp(
            -32000,
            "eth_getProof is not supported on this plan"
        )));
        assert!(!method_unsupported(&resp(-32005, "rate limit exceeded")));
        assert!(!method_unsupported(&TransportErrorKind::custom_str(
            "method not found"
        )));
    }

    /// The batch read must stay usable from spawned tasks: its future is
    /// Send. Compile-time regression test — no RPC is made (PerpClient::new
    /// performs no network calls and the future is never polled).
    #[test]
    fn get_maker_equities_future_is_send() {
        fn require_send<T: Send>(_: &T) {}

        let (client, _rpc) = mock::client();

        let ids = [U256::ONE];
        let fut = client.market().get_maker_equities(&ids);
        require_send(&fut);
        drop(fut);
    }

    // ── The maker rows ────────────────────────────────────────────────

    /// A position row holding this margin, and a maker row with this
    /// liquidity over `[-60, 60]`.
    fn rows(margin: u128, liquidity: u128) -> [IMulticall3::Result; 2] {
        [
            mock::ok_row(returns::<Perp::positionsCall>(&mock::position(margin))),
            mock::ok_row(returns::<Perp::makerDetailsCall>(&mock::maker(
                -60, 60, liquidity,
            ))),
        ]
    }

    /// One bad row degrades alone: the other ids in the batch keep their
    /// open-maker / not-a-maker reading and their input order.
    #[tokio::test]
    async fn maker_rows_degrade_one_bad_row_alone() {
        let (state, rpc) = state().await;
        let pos_ids = [U256::from(11u8), U256::from(22u8), U256::from(33u8)];
        let [p11, m11] = rows(1_000_000, 1_000);
        let [p22, _] = rows(1_000_000, 1_000);
        let [p33, m33] = rows(1_000_000, 0);
        rpc.aggregate3(vec![p11, m11, p22, mock::failed_row(), p33, m33]);

        let rows = state.maker_rows(&pos_ids).await;
        assert_eq!(rows.len(), 3);
        let (position, details) = rows[0].row.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(
            (rows[0].pos_id, position.margin, details.liquidity),
            (pos_ids[0], 1_000_000, 1_000)
        );
        let Err(err) = &rows[1].row else {
            panic!("pos 22's maker row reverted");
        };
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::MulticallFailed { .. })
            ),
            "{err}"
        );
        assert!(!err.is_transient(), "a reverted row reverts again");
        assert!(rows[2].row.as_ref().unwrap().is_none(), "zero liquidity");
        assert!(rpc.is_drained(), "one multicall");
    }

    // ── The whole batch, on one handle ────────────────────────────────

    /// The market-wide answers: the immutables, the eight views in one
    /// multicall at block 92 with every price 1.0 and the market last
    /// touched at the block's timestamp, then the beacon's index.
    fn market_answers(rpc: &Rpc) {
        let one = x96(1, 0);
        rpc.call::<Perp::POOL_IDCall>(&POOL_ID);
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key(30));
        rpc.aggregate(
            92,
            [
                returns::<Perp::modulesCall>(&mock::modules()),
                returns::<Perp::poolStateCall>(&Perp::poolStateReturn {
                    sqrtPrice: Uint::from(1u8) << 96,
                    ..mock::pool_state(one)
                }),
                returns::<Perp::emasCall>(&mock::emas(one.to::<u128>(), one.to::<u128>())),
                returns::<Perp::ratesCall>(&Rates {
                    lastTouch: Uint::from(TIMESTAMP),
                    ..mock::rates(0)
                }),
                returns::<Perp::EMA_WINDOWCall>(&U256::from(3_600u32)),
                returns::<Perp::cumulativesCall>(&Cumulatives {
                    fundingX96: I256::ZERO,
                    fundingDivSqrtPX96: I256::ZERO,
                    longUtilEarningsX96: U256::ZERO,
                    shortUtilEarningsX96: U256::ZERO,
                    longUtilPaymentsX96: U256::ZERO,
                    shortUtilPaymentsX96: U256::ZERO,
                }),
                returns::<Perp::capacityCall>(&mock::capacity(0, 0)),
                returns::<Perp::openInterestCall>(&mock::open_interest(0, 0)),
            ],
        );
        rpc.call::<IBeacon::indexCall>(&one);
    }

    /// The two funding words of each of `ticks`, all zero, as one
    /// `eth_getProof` answer.
    fn proof_answers(rpc: &Rpc, ticks: &[i32]) {
        rpc.proof(
            PERP,
            ticks
                .iter()
                .flat_map(|&tick| perp_tick_funding_slots(tick))
                .map(|slot| (slot, U256::ZERO)),
        );
    }

    /// Every id gets its outcome from one handle's block: the open maker
    /// is computed from its rows and the two storage reads, the reverted
    /// row fails alone and deterministically, and the zero-liquidity row
    /// is not a maker. The request sequence is the whole cost of the batch.
    #[tokio::test]
    async fn maker_equities_read_everything_at_the_handles_block() {
        let (state, rpc) = state().await;
        let pos_ids = [U256::from(11u8), U256::from(22u8), U256::from(33u8)];
        market_answers(&rpc);
        let [p11, m11] = rows(1_000_000, 1_000);
        let [p22, _] = rows(1_000_000, 1_000);
        let [p33, m33] = rows(1_000_000, 0);
        rpc.aggregate3(vec![p11, m11, p22, mock::failed_row(), p33, m33]);
        // 1 global + 2 ticks + 1 position, all zero.
        rpc.call::<IPoolManagerState::extsload_1Call>(&vec![B256::ZERO; 4]);
        proof_answers(&rpc, &[-60, 60]);

        let outcomes = state.maker_equities(&pos_ids).await.unwrap();
        assert_eq!(outcomes.len(), 3);
        let MakerEquityKind::Computed(breakdown) = &outcomes[0].kind else {
            panic!("pos 11 is an open maker: {:?}", outcomes[0].kind);
        };
        assert_eq!(breakdown.margin_atoms(), 1_000_000);
        let MakerEquityKind::Failed(err) = &outcomes[1].kind else {
            panic!("pos 22's row reverted: {:?}", outcomes[1].kind);
        };
        assert!(!err.is_transient(), "{err}");
        assert!(matches!(outcomes[2].kind, MakerEquityKind::NotAMaker));
        assert!(
            rpc.is_drained(),
            "immutables, one multicall, the index, the rows, the extsload, the proof"
        );
    }

    /// A chunk's shared fee-growth read failing at a block the replica
    /// lacks fails the chunk's open makers with the typed, transient
    /// answer, and leaves the ids the row read already settled alone.
    #[tokio::test]
    async fn a_failed_fee_growth_read_fails_the_chunks_makers_with_the_block_named() {
        let (state, rpc) = state().await;
        let pos_ids = [U256::from(11u8), U256::from(22u8), U256::from(33u8)];
        market_answers(&rpc);
        let [p11, m11] = rows(1_000_000, 1_000);
        let [p22, _] = rows(1_000_000, 1_000);
        let [p33, m33] = rows(1_000_000, 0);
        rpc.aggregate3(vec![p11, m11, p22, mock::failed_row(), p33, m33]);
        rpc.fails("header not found");
        proof_answers(&rpc, &[-60, 60]);

        let outcomes = state.maker_equities(&pos_ids).await.unwrap();
        let MakerEquityKind::Failed(err) = &outcomes[0].kind else {
            panic!("pos 11's fee-growth read failed: {:?}", outcomes[0].kind);
        };
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::BlockUnavailable { number: 92 })
            ),
            "{err}"
        );
        assert!(err.is_transient());
        assert!(matches!(outcomes[1].kind, MakerEquityKind::Failed(_)));
        assert!(
            matches!(outcomes[2].kind, MakerEquityKind::NotAMaker),
            "a row the multicall answered keeps its answer"
        );
        assert!(rpc.is_drained());
    }

    /// A what-if mark of zero is refused before anything is read.
    #[tokio::test]
    async fn a_zero_what_if_mark_is_refused_before_any_read() {
        let (state, rpc) = state().await;
        let err = state
            .maker_equities_at_mark(&[U256::ONE], U256::ZERO)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            PerpCityError::Validation(ValidationError::InvalidPrice { .. })
        ));
        assert!(rpc.is_drained());
    }

    // ── Tick funding: the eth_getProof latch and the storage fallback ─

    /// One tick's two funding words, as the storage fallback reads them:
    /// the opposite cumulative, then the one over sqrt price.
    fn funding_words(rpc: &Rpc, opp: u8, div: u8) {
        rpc.storage(U256::from(opp));
        rpc.storage(U256::from(div));
    }

    /// The proof path: one request, every tick's two words from its
    /// storage proof.
    #[tokio::test]
    async fn eth_getproof_serves_every_tick_in_one_request() {
        let ticks = BTreeSet::from([-60, 60]);
        let (state, rpc) = state().await;
        let [opp, div] = perp_tick_funding_slots(60);
        rpc.proof(
            PERP,
            perp_tick_funding_slots(-60)
                .map(|slot| (slot, U256::ZERO))
                .into_iter()
                .chain([(opp, U256::from(7u8)), (div, U256::from(9u8))]),
        );

        let funding = state.tick_funding(&ticks).await;
        assert_eq!(*funding[&-60].as_ref().unwrap(), TickFunding::default());
        let read = funding[&60].as_ref().unwrap();
        assert_eq!(read.cuml_funding_opp_x96, I256::from_raw(U256::from(7u8)));
        assert_eq!(
            read.cuml_funding_div_sqrt_p_opp_x96,
            I256::from_raw(U256::from(9u8))
        );
        assert!(rpc.is_drained(), "one proof, no storage reads");
    }

    /// A proof that omits a tick's slots degrades that tick alone, and
    /// retryably: a replica may have dropped part of the proof.
    #[tokio::test]
    async fn a_proof_missing_a_ticks_slots_degrades_that_tick_alone() {
        let ticks = BTreeSet::from([-60, 60]);
        let (state, rpc) = state().await;
        rpc.proof(
            PERP,
            perp_tick_funding_slots(-60).map(|slot| (slot, U256::ZERO)),
        );

        let funding = state.tick_funding(&ticks).await;
        assert!(funding[&-60].is_ok());
        let err = tick_funding_for(&funding, 60).unwrap_err();
        assert!(err.is_transient(), "{err}");
        assert!(rpc.is_drained());
    }

    /// An endpoint that answers `eth_getProof` with "method not found" is
    /// remembered: the fallback serves that read, and the next read goes
    /// straight to storage without probing again.
    #[tokio::test]
    async fn a_method_not_found_on_eth_getproof_latches_the_storage_fallback() {
        let ticks = BTreeSet::from([60]);
        let (state, rpc) = state().await;
        rpc.method_not_found();
        funding_words(&rpc, 7, 9);

        let funding = state.tick_funding(&ticks).await;
        let read = funding[&60].as_ref().unwrap();
        assert_eq!(read.cuml_funding_opp_x96, I256::from_raw(U256::from(7u8)));
        assert_eq!(
            read.cuml_funding_div_sqrt_p_opp_x96,
            I256::from_raw(U256::from(9u8))
        );
        assert!(rpc.is_drained(), "the probe, then two storage reads");

        funding_words(&rpc, 7, 9);
        let funding = state.tick_funding(&ticks).await;
        assert!(funding[&60].is_ok());
        assert!(rpc.is_drained(), "latched: two storage reads and no probe");
    }

    /// Any other `eth_getProof` failure falls back for that read alone and
    /// is not held against the next one, which probes again.
    #[tokio::test]
    async fn any_other_eth_getproof_failure_falls_back_without_latching() {
        let ticks = BTreeSet::from([60]);
        let (state, rpc) = state().await;
        for _ in 0..2 {
            rpc.fails("rate limited");
            funding_words(&rpc, 7, 9);

            let funding = state.tick_funding(&ticks).await;
            assert!(funding[&60].is_ok());
            assert!(rpc.is_drained(), "probed again: three answers each time");
        }
    }

    /// A tick whose storage read fails is reported failed on its own, with
    /// the transport cause kept, while the other ticks' reads stand.
    #[tokio::test]
    async fn a_failed_tick_read_degrades_that_tick_alone() {
        let ticks = BTreeSet::from([-60, 60]);
        let (state, rpc) = state().await;
        rpc.method_not_found();
        funding_words(&rpc, 7, 9);
        rpc.storage(U256::from(7u8));
        rpc.fails("replica dropped the read");

        let funding = state.tick_funding(&ticks).await;
        assert!(funding[&-60].is_ok());
        assert!(funding[&60].is_err());
        assert!(rpc.is_drained());
    }

    /// The latch belongs to the transport, not the client: a second client
    /// over the same transport inherits what the first learned and goes
    /// straight to storage, with no probe of its own.
    #[tokio::test]
    async fn the_latch_is_shared_by_every_client_over_one_transport() {
        let ticks = BTreeSet::from([60]);
        let transport = mock::transport();
        let (first, first_rpc) = mock::client_sharing(transport.clone());
        let (second, second_rpc) = mock::client_sharing(transport);

        first_rpc.block(92, TIMESTAMP);
        let first = first.market().state_at(92).await.unwrap();
        first_rpc.method_not_found();
        funding_words(&first_rpc, 7, 9);
        first.tick_funding(&ticks).await;

        second_rpc.block(92, TIMESTAMP);
        let second = second.market().state_at(92).await.unwrap();
        funding_words(&second_rpc, 7, 9);
        let funding = second.tick_funding(&ticks).await;
        assert!(funding[&60].is_ok());
        assert!(second_rpc.is_drained(), "two storage reads and no probe");
    }
}
