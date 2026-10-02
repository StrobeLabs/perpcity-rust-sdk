//! A beacon's index series, rebuilt from its `IndexUpdated` logs.

use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::SolEvent;
use serde::{Deserialize, Serialize};

use crate::contracts::IBeacon;
use crate::errors::{Result, ValidationError};
use crate::events::decode_raw;
use crate::units::Price;

use futures_util::TryStreamExt;

use super::scan::{SharedWidths, block_timestamps, check_block_range, scan_all, scan_newest};

/// One index value a beacon published.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexPrint {
    /// Block the print landed in.
    pub block_number: u64,
    /// Position of the print's log in its block.
    pub log_index: u64,
    /// Unix timestamp of the block.
    pub timestamp: u64,
    /// The printed index, exactly as the beacon emitted it. The same type
    /// the live [`IndexUpdated`](crate::MarketEvent::IndexUpdated) carries,
    /// so a fold over an index series does not care which tense it came
    /// from.
    pub index: Price,
}

impl IndexPrint {
    /// The printed index as a float.
    ///
    /// # Errors
    ///
    /// As [`Price::to_f64`]: a zero print, or one beyond the safe f64 range.
    pub fn index_f64(&self) -> std::result::Result<f64, ValidationError> {
        self.index.to_f64()
    }
}

/// Every index print `beacon` published in blocks `from_block..=to_block`,
/// oldest first.
///
/// # Errors
///
/// [`ValidationError::InvalidConfig`] for the zero address,
/// [`ValidationError::InvalidBlockRange`] if `from_block > to_block`,
/// [`ContractError::BlockUnavailable`](crate::errors::ContractError::BlockUnavailable)
/// if a print's block header is missing
/// when the provider omits log timestamps,
/// [`ValidationError::DecodeFailed`] for an `IndexUpdated` log that does
/// not decode, or an error from [`get_logs_chunked`](super::get_logs_chunked).
pub async fn beacon_prints<P: Provider>(
    provider: &P,
    beacon: Address,
    from_block: u64,
    to_block: u64,
) -> Result<Vec<IndexPrint>> {
    beacon_prints_with(
        provider,
        beacon,
        from_block,
        to_block,
        &SharedWidths::new(),
        1,
    )
    .await
}

/// [`beacon_prints`] over a caller-held width search, with up to
/// `in_flight` window requests outstanding.
pub(super) async fn beacon_prints_with<P: Provider>(
    provider: &P,
    beacon: Address,
    from_block: u64,
    to_block: u64,
    widths: &SharedWidths,
    in_flight: usize,
) -> Result<Vec<IndexPrint>> {
    let filter = index_updated_filter(beacon)?;
    let logs = scan_all(provider, &filter, from_block, to_block, widths, in_flight).await?;
    with_timestamps(provider, &logs).await
}

/// The newest `limit` index prints `beacon` published in blocks
/// `from_block..=to_block`, oldest first.
///
/// Reads backward from `to_block` and stops once it holds `limit` prints,
/// so `from_block` is only a floor: a beacon's creation block, or any
/// block known to be before it, is safe.
///
/// # Errors
///
/// As [`beacon_prints`].
pub async fn latest_beacon_prints<P: Provider>(
    provider: &P,
    beacon: Address,
    from_block: u64,
    to_block: u64,
    limit: usize,
) -> Result<Vec<IndexPrint>> {
    latest_beacon_prints_with(
        provider,
        beacon,
        from_block,
        to_block,
        limit,
        &SharedWidths::new(),
    )
    .await
}

/// [`latest_beacon_prints`] over a caller-held width search.
pub(super) async fn latest_beacon_prints_with<P: Provider>(
    provider: &P,
    beacon: Address,
    from_block: u64,
    to_block: u64,
    limit: usize,
    widths: &SharedWidths,
) -> Result<Vec<IndexPrint>> {
    let filter = index_updated_filter(beacon)?;
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
        held += chunk.len();
        newest_first.push(chunk);
    }
    let logs: Vec<Log> = newest_first.into_iter().rev().flatten().collect();
    let skip = logs.len().saturating_sub(limit);
    with_timestamps(provider, &logs[skip..]).await
}

fn index_updated_filter(beacon: Address) -> std::result::Result<Filter, ValidationError> {
    if beacon.is_zero() {
        return Err(ValidationError::InvalidConfig {
            reason: "beacon address is zero".into(),
        });
    }
    Ok(Filter::new()
        .address(beacon)
        .event_signature(IBeacon::IndexUpdated::SIGNATURE_HASH))
}

/// Decodes `IndexUpdated` logs into prints, reading the block header for
/// any log that arrived without a timestamp.
async fn with_timestamps<P: Provider>(provider: &P, logs: &[Log]) -> Result<Vec<IndexPrint>> {
    let headers = block_timestamps(provider, logs.iter()).await?;
    logs.iter()
        .map(|log| {
            let decoded = log
                .block_number
                .zip(log.log_index)
                .and_then(|(block, index)| {
                    // The surrounding `ok_or_else` already names this log and
                    // its transaction, which beats the signature alone.
                    let event = decode_raw::<IBeacon::IndexUpdated>(log).ok()?;
                    Some((block, index, event.index))
                });
            let (block_number, log_index, index) =
                decoded.ok_or_else(|| ValidationError::DecodeFailed {
                    context: format!(
                        "IndexUpdated log from {} in tx {:?}",
                        log.address(),
                        log.transaction_hash
                    ),
                })?;
            let timestamp = match log.block_timestamp {
                Some(timestamp) => timestamp,
                None => headers[&block_number],
            };
            Ok(IndexPrint {
                block_number,
                log_index,
                timestamp,
                index: Price::from_x96(index),
            })
        })
        .collect()
}
