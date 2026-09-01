//! The maker-equity batch read: block-pinned settle previews for a set of
//! position ids.
//!
//! The pure settlement math lives in [`crate::math::maker_equity`]; this
//! module is the chain-read layer that feeds it — the market-wide
//! multicall, the per-position row multicall, the PoolManager `extsload`
//! over the V4 fee-growth slots, and the `eth_getProof` /
//! `eth_getStorageAt` reads of the Perp tick-funding slots, all pinned to
//! one lagged block.

use std::collections::{BTreeMap, BTreeSet};
use std::future::IntoFuture;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use alloy::eips::BlockId;
use alloy::primitives::{Address, B256, I256, U256};
use alloy::providers::Provider;
use alloy::sol_types::SolCall;
use alloy::transports::{TransportError, TransportErrorKind};
use futures_util::stream::{self, StreamExt};

use crate::constants::MULTICALL3;
use crate::contracts::{IMulticall3, IPoolManagerState, Maker, Perp, Position};
use crate::convert::unpack_balance_delta;
use crate::errors::{ContractError, PerpCityError, Result, ValidationError};
use crate::math::maker_equity::{
    AccrualInputs, AccruedMakerSnapshot, MakerEquityBreakdown, MakerMarketSnapshot, MakerState,
    TickFunding, fee_growth_inside1,
};
use crate::math::storage::{
    perp_tick_funding_slots, v4_fee_growth_global1_slot, v4_position_fee_growth_inside1_slot,
    v4_tick_fee_growth_outside1_slot,
};
use crate::math::tick::get_sqrt_ratio_at_tick;

use super::{PerpClient, i24_to_i32};

/// Concurrency bound for the `eth_getStorageAt` fallback when the endpoint
/// does not serve `eth_getProof`.
const TICK_READ_CONCURRENCY: usize = 16;

/// Outcome of one tick's funding read: the two decoded words, or the shared
/// transport error that failed every position referencing the tick.
type TickFundingRead = std::result::Result<TickFunding, Arc<TransportError>>;

/// Maximum position ids per RPC batch inside a maker-equity read.
///
/// [`PerpClient::get_maker_equities`] chunks larger inputs internally at
/// this size (every chunk still pins to the one shared block), keeping
/// each row multicall and slot read inside RPC response-size and calldata
/// limits. Exposed so callers sizing their own sweeps can align with it.
pub const MAX_MAKER_EQUITY_BATCH: usize = 500;

/// The batch outcome for one requested position id: every id passed to
/// [`PerpClient::get_maker_equities`] comes back as exactly one of these,
/// in input order.
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

    fn word_count(&self) -> usize {
        self.word_count
    }

    /// Word 0 is the pool's `feeGrowthGlobal1X128` by construction.
    fn global(words: &[B256]) -> U256 {
        U256::from_be_bytes(words[0].0)
    }

    /// `feeGrowthOutside1X128` of `tick`, or `None` when the tick was not a
    /// band boundary of the positions the layout was built from.
    fn outside(&self, words: &[B256], tick: i32) -> Option<U256> {
        self.tick_word
            .get(&tick)
            .map(|&word| U256::from_be_bytes(words[word].0))
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

/// Provider-specific message substrings that also mean "this RPC method is
/// not served here". A heuristic: providers spell the condition many ways
/// and some return generic codes, so the match is deliberately loose —
/// worst case a genuine failure latches the (always-correct)
/// `eth_getStorageAt` fallback.
const METHOD_UNSUPPORTED_MARKERS: [&str; 4] = [
    "not supported",
    "unsupported",
    "method not found",
    "does not exist",
];

/// Whether a transport error means the RPC method itself is not available
/// on the endpoint, as opposed to the call failing.
fn method_unsupported(e: &TransportError) -> bool {
    e.as_error_resp().is_some_and(|resp| {
        resp.code == METHOD_NOT_FOUND || {
            let message = resp.message.to_lowercase();
            METHOD_UNSUPPORTED_MARKERS
                .iter()
                .any(|marker| message.contains(marker))
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
    let (row_pairs, _) = rows.as_chunks::<2>();
    for (input_index, ([position_row, details_row], &pos_id)) in
        row_pairs.iter().zip(pos_ids).enumerate()
    {
        match decode_maker_row(input_index, pos_id, position_row, details_row) {
            Ok(None) => {}
            Ok(Some(maker)) => pending.push(maker),
            Err(e) => {
                tracing::debug!(%pos_id, error = %e, "maker equity: position row failed");
                failed.push((input_index, pos_id, e));
            }
        }
    }
    (pending, failed)
}

impl PerpClient {
    /// Read chain state and compute the settle-preview equity for each maker
    /// position in `pos_ids`, all pinned to one block.
    ///
    /// Returns exactly one [`MakerEquityOutcome`] per input id, in input
    /// order: [`Computed`](MakerEquityKind::Computed) for an open maker,
    /// [`NotAMaker`](MakerEquityKind::NotAMaker) for zero-liquidity ids
    /// (takers, burned, never minted), and
    /// [`Failed`](MakerEquityKind::Failed) when that one position's reads,
    /// decoding, or settle math failed — the rest of the batch is
    /// unaffected. Market-wide read failures still fail the whole call.
    ///
    /// A `Failed` outcome is worth retrying exactly when its error's
    /// [`PerpCityError::is_transient`] is true (a lagging replica or a
    /// dropped storage read); decode and settle-math failures are
    /// deterministic and will not clear on retry.
    ///
    /// Reads are batched: one Multicall3 round trip for the market-wide
    /// state, one for the position/maker rows, one PoolManager `extsload`
    /// for the V4 fee-growth slots (distinct band ticks read once), and one
    /// `eth_getProof` for the distinct Perp tick-funding slots (with an
    /// `eth_getStorageAt` fallback on endpoints without `eth_getProof`).
    /// Inputs larger than [`MAX_MAKER_EQUITY_BATCH`] are chunked internally,
    /// every chunk pinned to the same block.
    ///
    /// The mark that prices `valPnl` and the accrual replay is read from
    /// `poolState().ammPrice` inside the same pinned multicall — exact X96,
    /// no float round-trip, and never a different block than the rest of
    /// the snapshot. For what-if pricing at a caller-chosen mark, use
    /// [`Self::get_maker_equities_at_mark`].
    pub async fn get_maker_equities(&self, pos_ids: &[U256]) -> Result<Vec<MakerEquityOutcome>> {
        self.get_maker_equities_inner(pos_ids, None).await
    }

    /// [`Self::get_maker_equities`] priced at a caller-supplied mark
    /// (exact X96) instead of the pinned `poolState().ammPrice` — what-if
    /// pricing for stress marks or off-snapshot scenarios. All chain state
    /// is still read at the pinned block; only the pricing input changes.
    pub async fn get_maker_equities_at_mark(
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
        self.get_maker_equities_inner(pos_ids, Some(mark_price_x96))
            .await
    }

    async fn get_maker_equities_inner(
        &self,
        pos_ids: &[U256],
        mark_override_x96: Option<U256>,
    ) -> Result<Vec<MakerEquityOutcome>> {
        if pos_ids.is_empty() {
            return Ok(Vec::new());
        }
        let (market, pool_id, block_id) =
            self.load_maker_market_snapshot(mark_override_x96).await?;

        let mut kinds = Vec::with_capacity(pos_ids.len());
        for chunk in pos_ids.chunks(MAX_MAKER_EQUITY_BATCH) {
            kinds.extend(
                self.get_chunk_equity_kinds(&market, pool_id, block_id, chunk)
                    .await?,
            );
        }

        tracing::debug!(
            count = pos_ids.len(),
            block = market.snapshot().block.number,
            "maker equities read"
        );
        Ok(pos_ids
            .iter()
            .zip(kinds)
            .map(|(&pos_id, kind)| MakerEquityOutcome { pos_id, kind })
            .collect())
    }

    /// One chunk of the batch: the row multicall plus the slot reads and
    /// math for its surviving positions, producing one
    /// [`MakerEquityKind`] per chunk id by input index.
    async fn get_chunk_equity_kinds(
        &self,
        market: &AccruedMakerSnapshot,
        pool_id: B256,
        block_id: BlockId,
        pos_ids: &[U256],
    ) -> Result<Vec<MakerEquityKind>> {
        let perp_addr = self.deployments.perp;

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
        let equities = self
            .get_pending_maker_equities(market, pool_id, block_id, &pending)
            .await?;

        // Fill by input index: ids that produced neither a pending maker
        // nor a failure were zero-liquidity rows.
        let mut kinds: Vec<MakerEquityKind> =
            pos_ids.iter().map(|_| MakerEquityKind::NotAMaker).collect();
        for (maker, equity) in pending.iter().zip(equities) {
            kinds[maker.input_index] = match equity {
                Ok(breakdown) => MakerEquityKind::Computed(breakdown),
                Err(e) => MakerEquityKind::Failed(e),
            };
        }
        for (input_index, _, e) in failed {
            kinds[input_index] = MakerEquityKind::Failed(e);
        }
        Ok(kinds)
    }

    /// Resolve the safe lagged block and load + accrue the market-wide
    /// snapshot for [`Self::get_maker_equities`] in one multicall.
    ///
    /// The accrual replay always runs at the multicall's own
    /// `poolState().ammPrice`; `mark_override_x96` then reprices the
    /// accrued snapshot for what-if pricing. Returns the accrued snapshot,
    /// the pool id, and the block id every later read must pin to.
    async fn load_maker_market_snapshot(
        &self,
        mark_override_x96: Option<U256>,
    ) -> Result<(AccruedMakerSnapshot, B256, BlockId)> {
        let perp_addr = self.deployments.perp;
        // A lagging replica may briefly miss the pinned header. That is a
        // failed read, not a degraded one: pinning by bare number would drop
        // the reorg protection, and substituting the local wall clock for
        // the block timestamp would silently skew the accrual replay (a
        // slow clock reads as zero accrual). A settlement preview must not
        // quietly degrade — fail and let the caller retry.
        let (block, block_id) = self.lagged_snapshot_block().await?;

        let market_call = |calldata: Vec<u8>| IMulticall3::Call3 {
            target: perp_addr,
            allowFailure: false,
            callData: calldata.into(),
        };

        // ── Market-wide state: one multicall, all-or-nothing ────────
        // (allowFailure: false — without a consistent market snapshot no
        // position's equity can be computed.)
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
            block,
            funding_x96: cumls.fundingX96,
            funding_div_sqrt_p_x96: cumls.fundingDivSqrtPX96,
            long_util_earnings_x96: cumls.longUtilEarningsX96,
            short_util_earnings_x96: cumls.shortUtilEarningsX96,
            tick: i24_to_i32(pool_state.tick),
            sqrt_price_x96: pool_state.sqrtPrice.to::<U256>(),
            mark_price_x96: pool_state.ammPrice,
        }
        .accrued(&AccrualInputs {
            funding_per_day_wad: i128::try_from(rates.fundingPerDay)
                .expect("int88 always fits i128"),
            long_util_fee_per_day_wad: rates.longUtilFeePerDay,
            short_util_fee_per_day_wad: rates.shortUtilFeePerDay,
            last_touch: rates.lastTouch.to::<u64>(),
            accrue_to: block.timestamp,
            oi_long_atoms: oi.long,
            oi_short_atoms: oi.short,
            cap_long_atoms: capacity.long,
            cap_short_atoms: capacity.short,
        })?;
        // The what-if mark is applied AFTER the replay: the elapsed accrual
        // happened at the chain's mark, and only the pricing legs are the
        // caller's to override.
        let market = match mark_override_x96 {
            Some(mark_price_x96) => market.with_mark(mark_price_x96),
            None => market,
        };
        Ok((market, pool_id, block_id))
    }

    /// Slot reads and math for the surviving positions of
    /// [`Self::get_maker_equities`]: one `extsload` batch for the V4
    /// fee-growth slots (distinct band ticks read once), one `eth_getProof`
    /// batch for the Perp tick-funding slots (also deduplicated), degrading
    /// per position on failure.
    async fn get_pending_maker_equities(
        &self,
        market: &AccruedMakerSnapshot,
        pool_id: B256,
        block_id: BlockId,
        pending: &[PendingMaker],
    ) -> Result<Vec<Result<MakerEquityBreakdown>>> {
        if pending.is_empty() {
            return Ok(Vec::new());
        }

        let (layout, slots) = FeeGrowthLayout::new(pool_id, self.deployments.perp, pending);
        let manager = IPoolManagerState::new(self.deployments.pool_manager, &self.provider);
        let words = manager.extsload_1(slots).block(block_id).call().await?;
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
        let fg1_global = FeeGrowthLayout::global(&words);

        // Positions share band boundaries (a maker ladder reuses each inner
        // tick twice), so read each distinct tick's two funding words once.
        let ticks: BTreeSet<i32> = pending
            .iter()
            .flat_map(|maker| [maker.tick_lower, maker.tick_upper])
            .collect();
        let tick_funding = self.get_tick_funding(block_id, &ticks).await?;
        let funding_for = |tick: i32| tick_funding_for(&tick_funding, tick);

        let equities = pending
            .iter()
            .enumerate()
            .map(|(i, maker)| {
                let tick_lower_funding = funding_for(maker.tick_lower)?;
                let tick_upper_funding = funding_for(maker.tick_upper)?;
                let outside = |tick: i32| {
                    layout.outside(&words, tick).ok_or_else(|| {
                        PerpCityError::from(ContractError::StorageReadFailed {
                            context: format!("tick {tick} missing from fee-growth layout"),
                            source: None,
                        })
                    })
                };
                let fg1_out_lower = outside(maker.tick_lower)?;
                let fg1_out_upper = outside(maker.tick_upper)?;
                let fg1_inside_last = layout.inside_last(&words, i);
                let (delta_amount0, delta_amount1) = unpack_balance_delta(maker.position.delta);
                let state = MakerState {
                    margin_atoms: maker.position.margin,
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
                        market.snapshot().tick,
                    ),
                    fee_growth_inside1_last_x128: fg1_inside_last,
                };
                market.maker_equity(&state).map_err(|e| {
                    tracing::debug!(
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
    /// `eth_getStorageAt` with bounded concurrency. On both paths a failed
    /// (or missing) tick read degrades only the positions referencing that
    /// tick.
    async fn get_tick_funding(
        &self,
        block_id: BlockId,
        ticks: &BTreeSet<i32>,
    ) -> Result<BTreeMap<i32, TickFundingRead>> {
        let perp_addr = self.deployments.perp;
        if !self.get_proof_unsupported.load(Ordering::Relaxed) {
            let keys: Vec<B256> = ticks
                .iter()
                .flat_map(|&tick| perp_tick_funding_slots(tick).map(B256::from))
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
        // Collected into a Vec first so the returned future's Send bound is
        // provable from the concrete future type, not the borrowing
        // iterator adapter.
        let tick_reads: Vec<_> = ticks
            .iter()
            .map(|&tick| {
                let [slot_opp, slot_div] = perp_tick_funding_slots(tick);
                let read = |slot: U256| {
                    self.provider
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
        Ok(funding)
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
        let (layout, slots) = FeeGrowthLayout::new(pool_id, perp, &pending);

        // 1 global + 4 distinct ticks + 3 positions.
        assert_eq!(layout.word_count(), 8);
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
        // build set answers None instead of panicking.
        let words: Vec<B256> = (0u8..8).map(B256::repeat_byte).collect();
        assert_eq!(
            FeeGrowthLayout::global(&words),
            U256::from_be_bytes(words[0].0)
        );
        assert_eq!(
            layout.outside(&words, 60),
            Some(U256::from_be_bytes(words[layout.tick_word[&60]].0))
        );
        assert_eq!(layout.outside(&words, 90), None);
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

    /// The batch read must stay usable from spawned tasks: its future is
    /// Send. Compile-time regression test — no RPC is made (PerpClient::new
    /// performs no network calls and the future is never polled).
    #[test]
    fn get_maker_equities_future_is_send() {
        fn require_send<T: Send>(_: &T) {}

        let transport = crate::transport::provider::HftTransport::new(
            crate::transport::config::TransportConfig::builder()
                .shared_endpoint("http://127.0.0.1:1")
                .build()
                .unwrap(),
        )
        .unwrap();
        let signer = alloy::signers::local::PrivateKeySigner::random();
        let client = PerpClient::new(
            transport,
            signer,
            crate::types::Deployments {
                perp: Address::repeat_byte(1),
                usdc: Address::repeat_byte(2),
                pool_manager: Address::repeat_byte(3),
            },
            super::super::ARBITRUM_SEPOLIA_CHAIN_ID,
        )
        .unwrap();

        let ids = [U256::ONE];
        let fut = client.get_maker_equities(&ids);
        require_send(&fut);
        drop(fut);
    }
}
