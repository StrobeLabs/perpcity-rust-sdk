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
//! and mid-life transfers, which is how a position id maps to its owner
//! over time. It does not include [`MarketEvent::IndexUpdated`] (the
//! beacon's address — see [`beacon_prints`](super::beacon_prints)) or
//! [`MarketEvent::ModifyLiquidity`] (the PoolManager's address).

use alloy::primitives::{Address, B256};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use serde::{Deserialize, Serialize};

use crate::errors::{Result, ValidationError};
use crate::events::{MarketEvent, decode_log};

use futures_util::TryStreamExt;

use super::scan::{SharedWidths, block_timestamps, check_block_range, scan_all, scan_newest};

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
