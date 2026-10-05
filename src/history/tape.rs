//! The market-event tape: a perp's event history, replayed through the
//! same decoder the live feed uses.
//!
//! A [`TapeEvent`] is a [`MarketEvent`] with its chain position, so a
//! stored tape row and a live feed event carry the same vocabulary — the
//! only difference is which transport delivered the log. Logs the decoder
//! does not recognize (ERC-721 approvals, admin events) are skipped, as
//! the live feed skips them.
//!
//! [`market_events`] covers what the perp itself emits. That includes
//! [`MarketEvent::PositionTransferred`] — the position NFT's mint, burn
//! and mid-life transfers — so who held a position at a given block is a
//! fold of the tape: [`OwnershipLog`]. Trade events name a position id
//! and never a wallet, so that fold is how a market's activity is
//! attributed to the addresses behind it. A market's state has two more
//! sources, at other addresses: its index, [`MarketEvent::IndexUpdated`]
//! from the beacon, and its liquidity, [`MarketEvent::ModifyLiquidity`] from
//! the PoolManager, which logs every perp pool's liquidity change under
//! the pool's id. [`market_tape`] reads all three in one chain order, so a
//! fold that rebuilds the market from its events has everything the chain
//! said about it; [`market_events`] stays the one-address scan.
//!
//! Chain order has two sources: within one response, every production
//! client returns `eth_getLogs` results by block then log index; across
//! responses, the scan reads its windows in range order. Folds that
//! depend on it say so — [`OwnershipLog::fold`] debug-asserts each
//! position's transfers arrive strictly increasing.

use std::collections::BTreeMap;
use std::result::Result as StdResult;

use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::SolEvent;
use serde::{Deserialize, Serialize};

use crate::contracts::IPoolManagerState;
use crate::errors::{Result, ValidationError};
use crate::events::{MarketEvent, decode_log};

use futures_util::TryStreamExt;

use super::fold::Fold;
use super::scan::{SharedWidths, block_timestamps, check_block_range, scan_all, scan_newest};

/// A position in the chain's total order: a block, then a log's index
/// within it.
///
/// Two events in the same block still compare, which is what a join
/// across series (a fill against the index print before it, a trade
/// against the transfer that handed the position over) needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChainPoint {
    /// Block the log landed in.
    pub block: u64,
    /// Position of the log within its block.
    pub log_index: u64,
}

/// One market event with its chain position.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TapeEvent {
    /// Block the event landed in.
    pub block_number: u64,
    /// Hash of that block, so a state rebuilt from the tape carries the
    /// same block identity a pinned read does.
    pub block_hash: B256,
    /// Position of the event's log in its block.
    pub log_index: u64,
    /// Unix timestamp of the block.
    pub timestamp: u64,
    /// Transaction that emitted the event.
    pub tx_hash: B256,
    /// The decoded event, as the live feed would have streamed it.
    pub event: MarketEvent,
}

impl TapeEvent {
    /// Where this event sits in the chain's total order.
    pub fn point(&self) -> ChainPoint {
        ChainPoint {
            block: self.block_number,
            log_index: self.log_index,
        }
    }

    /// `event`, decoded from `log`, stamped with the log's mined position
    /// and `timestamp`: the one constructor both tenses use, so a feed's
    /// event and a scan's are the same row.
    ///
    /// # Errors
    ///
    /// [`ValidationError::DecodeFailed`] for a log without a mined
    /// position — a pending log, which neither tense should hand here.
    pub(crate) fn stamped(
        log: &Log,
        event: MarketEvent,
        timestamp: u64,
    ) -> StdResult<Self, ValidationError> {
        let position = log
            .block_number
            .zip(log.block_hash)
            .zip(log.log_index)
            .zip(log.transaction_hash);
        let (((block_number, block_hash), log_index), tx_hash) =
            position.ok_or_else(|| ValidationError::DecodeFailed {
                context: format!(
                    "market event log from {} in tx {:?} has no mined position",
                    log.address(),
                    log.transaction_hash
                ),
            })?;
        Ok(Self {
            block_number,
            block_hash,
            log_index,
            timestamp,
            tx_hash,
            event,
        })
    }
}

/// The three addresses a market's tape is read from.
///
/// A market's events come from its own contract; its index from the
/// beacon it is configured with; its liquidity from the shared PoolManager,
/// which logs every pool's liquidity under the pool's id. The client knows
/// all three ([`MarketReader::tape_addresses`](crate::MarketReader::tape_addresses));
/// the handle only needs to be told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TapeAddresses {
    /// The market.
    pub perp: Address,
    /// The beacon the market reads its index from.
    pub beacon: Address,
    /// The chain's PoolManager.
    pub pool_manager: Address,
    /// The market's pool, as the PoolManager keys it.
    pub pool_id: B256,
}

/// Who held each of a market's positions, over time: the custody
/// timeline folded from a tape's [`MarketEvent::PositionTransferred`]
/// events.
///
/// A mint is a transfer from the zero address and a burn (a full close,
/// or a liquidation) is a transfer to it, so custody is `None` before a
/// position's mint and after its burn.
///
/// ```
/// use perpcity_sdk::history::OwnershipLog;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let tape: Vec<perpcity_sdk::history::TapeEvent> = Vec::new();
/// let custody = OwnershipLog::fold(&tape);
/// for pos_id in custody.positions() {
///     let _owner = custody.latest_owner(pos_id);
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OwnershipLog {
    /// Position id → `(from this point on, this address holds it)`, in
    /// chain order. The zero address marks a burn.
    spans: BTreeMap<U256, Vec<(ChainPoint, Address)>>,
}

impl Fold for OwnershipLog {
    fn apply(&mut self, event: &TapeEvent) {
        if let MarketEvent::PositionTransferred { to, pos_id, .. } = event.event {
            let timeline = self.spans.entry(pos_id).or_default();
            let point = event.point();
            debug_assert!(
                timeline.last().is_none_or(|&(last, _)| last < point),
                "tape out of chain order at {point:?}"
            );
            timeline.push((point, to));
        }
    }

    fn combine(&mut self, later: Self) {
        for (pos_id, timeline) in later.spans {
            self.spans.entry(pos_id).or_default().extend(timeline);
        }
    }
}

impl OwnershipLog {
    /// Fold the custody timeline out of a tape's transfer events; other
    /// events are skipped.
    ///
    /// `events` must be in chain order, as every reader in this module
    /// returns them. The same as [`Fold::fold`], kept inherent so the fold
    /// is reachable without importing the trait.
    pub fn fold<'a>(events: impl IntoIterator<Item = &'a TapeEvent>) -> Self {
        <Self as Fold>::fold(events)
    }

    /// Who held `pos_id` at `at`: the recipient of its latest transfer at
    /// or before that point. `None` before the mint, after the burn, or
    /// for a position this tape never saw minted.
    ///
    /// This is the attribution a measurement over past events wants: a
    /// position handed between wallets mid-life has more than one owner,
    /// and only a point-in-time read credits each event to the wallet
    /// that held it then.
    pub fn owner_at(&self, pos_id: U256, at: ChainPoint) -> Option<Address> {
        let timeline = self.spans.get(&pos_id)?;
        let held = timeline.partition_point(|&(point, _)| point <= at);
        let (_, owner) = timeline[..held].last()?;
        (!owner.is_zero()).then_some(*owner)
    }

    /// The last wallet that held `pos_id`, ignoring its burn — the
    /// position's final owner, for a caller that wants one address per
    /// position rather than a timeline. `None` for a position this tape
    /// never saw minted.
    pub fn latest_owner(&self, pos_id: U256) -> Option<Address> {
        self.spans
            .get(&pos_id)?
            .iter()
            .rev()
            .map(|&(_, owner)| owner)
            .find(|owner| !owner.is_zero())
    }

    /// Every transfer of `pos_id`, oldest first, as
    /// `(point, new owner)`; the zero address ends the position.
    pub fn transfers(&self, pos_id: U256) -> impl Iterator<Item = (ChainPoint, Address)> + '_ {
        self.spans.get(&pos_id).into_iter().flatten().copied()
    }

    /// Every position id with custody history, ascending.
    pub fn positions(&self) -> impl Iterator<Item = U256> + '_ {
        self.spans.keys().copied()
    }

    /// Positions with custody history.
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// Whether the tape held no position transfers at all.
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}

/// Every market event `perp` emitted in blocks `from_block..=to_block`,
/// in chain order.
///
/// # Errors
///
/// [`ValidationError::InvalidConfig`] for the zero address,
/// [`ValidationError::InvalidBlockRange`] if `from_block > to_block`,
/// [`ContractError::BlockUnavailable`](crate::errors::ContractError::BlockUnavailable)
/// if an event's block header is missing when the provider omits log
/// timestamps, [`ValidationError::DecodeFailed`] for a recognized event
/// log missing its mined position, or an error from
/// [`get_logs_chunked`](super::get_logs_chunked).
pub async fn market_events<P: Provider>(
    provider: &P,
    perp: Address,
    from_block: u64,
    to_block: u64,
) -> Result<Vec<TapeEvent>> {
    market_events_with(
        provider,
        perp,
        from_block,
        to_block,
        &SharedWidths::new(),
        1,
    )
    .await
}

/// [`market_events`] over a caller-held width search, with up to
/// `in_flight` window requests outstanding.
pub(super) async fn market_events_with<P: Provider>(
    provider: &P,
    perp: Address,
    from_block: u64,
    to_block: u64,
    widths: &SharedWidths,
    in_flight: usize,
) -> Result<Vec<TapeEvent>> {
    let filter = perp_filter(perp)?;
    let logs = scan_all(provider, &filter, from_block, to_block, widths, in_flight).await?;
    tape_rows(provider, decode_known(logs, widths)).await
}

/// Everything the chain said about a market in blocks
/// `from_block..=to_block`, in one chain order: the perp's own events, the
/// beacon's prints and the PoolManager's liquidity changes for the market's
/// pool.
///
/// One range walk serves all three. The perp and the beacon share a filter
/// by address; the PoolManager is every pool on the chain, so its filter
/// names `ModifyLiquidity` and the pool id it indexes by. The two scans
/// share the learned width, and the rows are merged on chain point.
///
/// The beacon is the one in `addresses`, for the whole range. A market
/// whose beacon governance swapped inside the range has its earlier
/// beacon's prints missing here, with the `ModuleSet` that swapped it on
/// the tape to say so; a fold that meets one counts the prints before it
/// as a gap. No live market has swapped its beacon yet.
///
/// # Errors
///
/// As [`market_events`]; a zero address in `addresses` is
/// [`ValidationError::InvalidConfig`].
pub async fn market_tape<P: Provider>(
    provider: &P,
    addresses: TapeAddresses,
    from_block: u64,
    to_block: u64,
) -> Result<Vec<TapeEvent>> {
    market_tape_with(
        provider,
        addresses,
        from_block,
        to_block,
        &SharedWidths::new(),
        1,
    )
    .await
}

/// [`market_tape`] over a caller-held width search, with up to `in_flight`
/// window requests outstanding per scan.
pub(super) async fn market_tape_with<P: Provider>(
    provider: &P,
    addresses: TapeAddresses,
    from_block: u64,
    to_block: u64,
    widths: &SharedWidths,
    in_flight: usize,
) -> Result<Vec<TapeEvent>> {
    let logs =
        market_logs_with(provider, addresses, from_block, to_block, widths, in_flight).await?;
    let decoded = decode_known(logs, widths);
    tape_rows(provider, decoded).await
}

/// The raw logs a market's tape is decoded from, in chain order: the
/// perp's and the beacon's by address, the PoolManager's by the liquidity
/// event and the pool id. As the node returned them: a tape stamps only
/// the logs it decodes, a recording stamps them all with
/// [`stamp_timestamps`].
pub(super) async fn market_logs_with<P: Provider>(
    provider: &P,
    addresses: TapeAddresses,
    from_block: u64,
    to_block: u64,
    widths: &SharedWidths,
    in_flight: usize,
) -> Result<Vec<Log>> {
    let (market, liquidity) = tape_filters(addresses)?;
    let (mut logs, liquidity_logs) = futures_util::try_join!(
        scan_all(provider, &market, from_block, to_block, widths, in_flight),
        scan_all(
            provider, &liquidity, from_block, to_block, widths, in_flight
        ),
    )?;
    logs.extend(liquidity_logs);
    logs.sort_by_key(|log| (log.block_number, log.log_index));
    Ok(logs)
}

/// Give every log its block's timestamp, read from the header once per
/// block when the node omitted it, so the logs decode with no node later.
///
/// # Errors
///
/// [`ContractError::BlockUnavailable`](crate::errors::ContractError::BlockUnavailable)
/// for a header the node does not have.
pub(super) async fn stamp_timestamps<P: Provider>(provider: &P, logs: &mut [Log]) -> Result<()> {
    let headers = block_timestamps(provider, logs.iter()).await?;
    for log in logs.iter_mut() {
        if log.block_timestamp.is_none()
            && let Some(number) = log.block_number
        {
            log.block_timestamp = headers.get(&number).copied();
        }
    }
    Ok(())
}

/// The newest `limit` market events `perp` emitted in blocks
/// `from_block..=to_block`, oldest first.
///
/// Reads backward from `to_block` and stops once it holds `limit`
/// decodable events (skipped logs do not count), so `from_block` is only
/// a floor.
///
/// # Errors
///
/// As [`market_events`].
pub async fn latest_market_events<P: Provider>(
    provider: &P,
    perp: Address,
    from_block: u64,
    to_block: u64,
    limit: usize,
) -> Result<Vec<TapeEvent>> {
    latest_market_events_with(
        provider,
        perp,
        from_block,
        to_block,
        limit,
        &SharedWidths::new(),
    )
    .await
}

/// [`latest_market_events`] over a caller-held width search.
pub(super) async fn latest_market_events_with<P: Provider>(
    provider: &P,
    perp: Address,
    from_block: u64,
    to_block: u64,
    limit: usize,
    widths: &SharedWidths,
) -> Result<Vec<TapeEvent>> {
    let filter = perp_filter(perp)?;
    check_block_range(from_block, to_block)?;
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut chunks = std::pin::pin!(scan_newest(provider, &filter, from_block, to_block, widths));
    let mut newest_first = Vec::new();
    let mut held = 0;
    while held < limit
        && let Some(chunk) = chunks.try_next().await?
    {
        let decoded = decode_known(chunk, widths);
        held += decoded.len();
        newest_first.push(decoded);
    }
    let decoded: Vec<(Log, MarketEvent)> = newest_first.into_iter().rev().flatten().collect();
    let mut events = tape_rows(provider, decoded).await?;
    let skip = events.len().saturating_sub(limit);
    Ok(events.split_off(skip))
}

fn perp_filter(perp: Address) -> StdResult<Filter, ValidationError> {
    if perp.is_zero() {
        return Err(ValidationError::InvalidConfig {
            reason: "perp address is zero".into(),
        });
    }
    Ok(Filter::new().address(perp))
}

/// The market's two filters: the perp and its beacon by address; the
/// PoolManager by the liquidity event and the pool id it indexes by.
fn tape_filters(addresses: TapeAddresses) -> StdResult<(Filter, Filter), ValidationError> {
    let TapeAddresses {
        perp,
        beacon,
        pool_manager,
        pool_id,
    } = addresses;
    for (address, what) in [
        (perp, "perp"),
        (beacon, "beacon"),
        (pool_manager, "pool manager"),
    ] {
        if address.is_zero() {
            return Err(ValidationError::InvalidConfig {
                reason: format!("{what} address is zero"),
            });
        }
    }
    let market = Filter::new().address(vec![perp, beacon]);
    let liquidity = Filter::new()
        .address(pool_manager)
        .event_signature(IPoolManagerState::ModifyLiquidity::SIGNATURE_HASH)
        .topic1(pool_id);
    Ok((market, liquidity))
}

/// Decodes the logs this vocabulary recognizes, each kept with its log.
///
/// A log of another vocabulary is skipped, which is what a filter on one
/// address returns plenty of. A log of *this* vocabulary that will not
/// decode is counted on the scan's [`ScanStats::undecodable`] and skipped
/// too: the scan is not failed over it, because one such log should not cost
/// a scan of millions of blocks, but the count is how a caller learns the
/// tape has a gap rather than being told nothing at all.
fn decode_known(logs: Vec<Log>, widths: &SharedWidths) -> Vec<(Log, MarketEvent)> {
    let mut undecodable = 0;
    let decoded = logs
        .into_iter()
        .filter_map(|log| match decode_log(&log) {
            Ok(Some(event)) => Some((log, event)),
            Ok(None) => None,
            Err(e) => {
                undecodable += 1;
                tracing::warn!(
                    error = %e,
                    block = ?log.block_number,
                    "log recognised but undecodable; the tape has a gap here"
                );
                None
            }
        })
        .collect();
    if undecodable > 0 {
        widths.undecodable(undecodable);
    }
    decoded
}

/// Builds tape rows from decoded logs, reading the block header for any
/// event whose log arrived without a timestamp.
async fn tape_rows<P: Provider>(
    provider: &P,
    decoded: Vec<(Log, MarketEvent)>,
) -> Result<Vec<TapeEvent>> {
    let headers = block_timestamps(provider, decoded.iter().map(|(log, _)| log)).await?;
    decoded
        .into_iter()
        .map(|(log, event)| {
            let timestamp = match (log.block_timestamp, log.block_number) {
                (Some(timestamp), _) => timestamp,
                (None, Some(number)) => headers[&number],
                (None, None) => {
                    return Err(ValidationError::DecodeFailed {
                        context: format!(
                            "market event log from {} in tx {:?} has no mined position",
                            log.address(),
                            log.transaction_hash
                        ),
                    }
                    .into());
                }
            };
            Ok(TapeEvent::stamped(&log, event, timestamp)?)
        })
        .collect()
}
