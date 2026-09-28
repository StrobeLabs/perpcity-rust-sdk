//! The market-event tape: a perp's event history, replayed through the
//! same decoder the live feed uses.
//!
//! A [`TapeEvent`] is a [`MarketEvent`] with its chain position, so a
//! stored tape row and a live feed event carry the same vocabulary — the
//! only difference is which transport delivered the log. Logs the decoder
//! does not recognize (ERC-721 approvals, admin events) are skipped, as
//! the live feed skips them.
//!
//! The tape covers what the perp itself emits. That includes
//! [`MarketEvent::PositionTransferred`] — the position NFT's mint, burn
//! and mid-life transfers — so who held a position at a given block is a
//! fold of the tape: [`OwnershipLog`]. Trade events name a position id
//! and never a wallet, so that fold is how a market's activity is
//! attributed to the addresses behind it. The tape does not include
//! [`MarketEvent::IndexUpdated`] (the beacon's address — see
//! [`beacon_prints`](super::beacon_prints)) or
//! [`MarketEvent::ModifyLiquidity`] (the PoolManager's address).
//!
//! Chain order has two sources: within one response, every production
//! client returns `eth_getLogs` results by block then log index; across
//! responses, the scan reads its windows in range order. Folds that
//! depend on it say so — [`OwnershipLog::fold`] debug-asserts each
//! position's transfers arrive strictly increasing.

use std::collections::BTreeMap;

use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use serde::{Deserialize, Serialize};

use crate::errors::{Result, ValidationError};
use crate::events::{MarketEvent, decode_log};

use futures_util::TryStreamExt;

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

impl OwnershipLog {
    /// Fold the custody timeline out of a tape's transfer events; other
    /// events are skipped.
    ///
    /// `events` must be in chain order, as every reader in this module
    /// returns them.
    pub fn fold<'a>(events: impl IntoIterator<Item = &'a TapeEvent>) -> Self {
        let mut spans: BTreeMap<U256, Vec<(ChainPoint, Address)>> = BTreeMap::new();
        for event in events {
            if let MarketEvent::PositionTransferred { to, pos_id, .. } = event.event {
                let timeline = spans.entry(pos_id).or_default();
                let point = event.point();
                debug_assert!(
                    timeline.last().is_none_or(|&(last, _)| last < point),
                    "tape out of chain order at {point:?}"
                );
                timeline.push((point, to));
            }
        }
        Self { spans }
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
    tape_rows(provider, decode_known(logs)).await
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
        let decoded = decode_known(chunk);
        held += decoded.len();
        newest_first.push(decoded);
    }
    let decoded: Vec<(Log, MarketEvent)> = newest_first.into_iter().rev().flatten().collect();
    let mut events = tape_rows(provider, decoded).await?;
    let skip = events.len().saturating_sub(limit);
    Ok(events.split_off(skip))
}

fn perp_filter(perp: Address) -> std::result::Result<Filter, ValidationError> {
    if perp.is_zero() {
        return Err(ValidationError::InvalidConfig {
            reason: "perp address is zero".into(),
        });
    }
    Ok(Filter::new().address(perp))
}

/// Decodes the logs the feed decoder recognizes, each kept with its log.
fn decode_known(logs: Vec<Log>) -> Vec<(Log, MarketEvent)> {
    logs.into_iter()
        .filter_map(|log| decode_log(&log).map(|event| (log, event)))
        .collect()
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
            let position = log
                .block_number
                .zip(log.log_index)
                .zip(log.transaction_hash);
            let ((block_number, log_index), tx_hash) =
                position.ok_or_else(|| ValidationError::DecodeFailed {
                    context: format!(
                        "market event log from {} in tx {:?}",
                        log.address(),
                        log.transaction_hash
                    ),
                })?;
            let timestamp = match log.block_timestamp {
                Some(timestamp) => timestamp,
                None => headers[&block_number],
            };
            Ok(TapeEvent {
                block_number,
                log_index,
                timestamp,
                tx_hash,
                event,
            })
        })
        .collect()
}
