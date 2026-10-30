//! The readers: a market's tape from its three addresses, through the
//! chunked scan and the live feed's decoder.

use std::collections::BTreeMap;
use std::result::Result as StdResult;

use alloy::primitives::{Address, B256};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::SolEvent;
use futures_util::TryStreamExt;

use crate::contracts::IPoolManagerState;
use crate::errors::{Result, ValidationError};
use crate::events::{MarketEvent, decode_log};
use crate::history::scan::{
    SharedWidths, block_timestamps, check_block_range, scan_all, scan_newest,
};

use super::{PoolLogs, Tape, TapeAddresses, TapeEvent};

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
/// [`get_logs_chunked`](crate::history::get_logs_chunked).
pub async fn market_events<P: Provider>(
    provider: &P,
    perp: Address,
    from_block: u64,
    to_block: u64,
) -> Result<Tape> {
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
pub(in crate::history) async fn market_events_with<P: Provider>(
    provider: &P,
    perp: Address,
    from_block: u64,
    to_block: u64,
    widths: &SharedWidths,
    in_flight: usize,
) -> Result<Tape> {
    let filter = perp_filter(perp)?;
    let logs = scan_all(provider, &filter, from_block, to_block, widths, in_flight).await?;
    tape_rows(provider, decode_known(logs, widths)).await
}

/// Everything the chain said about a market in blocks
/// `from_block..=to_block`, in one chain order: the perp's own events, the
/// beacon's prints and the PoolManager's events for the market's pool: its
/// creation, its swaps and its liquidity changes.
///
/// One range walk serves all three. The perp and the beacon share a filter
/// by address; the PoolManager is every pool on the chain, so its filter
/// names the three pool events and the pool id they index by. The two scans
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
) -> Result<Tape> {
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
pub(in crate::history) async fn market_tape_with<P: Provider>(
    provider: &P,
    addresses: TapeAddresses,
    from_block: u64,
    to_block: u64,
    widths: &SharedWidths,
    in_flight: usize,
) -> Result<Tape> {
    let logs =
        market_logs_with(provider, addresses, from_block, to_block, widths, in_flight).await?;
    let decoded = decode_known(logs, widths);
    tape_rows(provider, decoded).await
}

/// The raw logs a market's tape is decoded from, in chain order: the
/// perp's and the beacon's by address, the PoolManager's by its pool
/// events and the pool id. As the node returned them: a tape stamps only
/// the logs it decodes, a recording stamps them all with
/// [`stamp_timestamps`].
pub(in crate::history) async fn market_logs_with<P: Provider>(
    provider: &P,
    addresses: TapeAddresses,
    from_block: u64,
    to_block: u64,
    widths: &SharedWidths,
    in_flight: usize,
) -> Result<Vec<Log>> {
    let (market, pool) = tape_filters(addresses)?;
    let (mut logs, pool_logs) = futures_util::try_join!(
        scan_all(provider, &market, from_block, to_block, widths, in_flight),
        scan_all(provider, &pool, from_block, to_block, widths, in_flight),
    )?;
    logs.extend(pool_logs);
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
pub(in crate::history) async fn stamp_timestamps<P: Provider>(
    provider: &P,
    logs: &mut [Log],
) -> Result<()> {
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
) -> Result<Tape> {
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
pub(in crate::history) async fn latest_market_events_with<P: Provider>(
    provider: &P,
    perp: Address,
    from_block: u64,
    to_block: u64,
    limit: usize,
    widths: &SharedWidths,
) -> Result<Tape> {
    let filter = perp_filter(perp)?;
    check_block_range(from_block, to_block)?;
    if limit == 0 {
        return Ok(Tape::default());
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
    let mut events = tape_rows(provider, decoded).await?.into_vec();
    let skip = events.len().saturating_sub(limit);
    Ok(Tape::new(events.split_off(skip))?)
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
/// PoolManager by its three pool events and the pool id they index by.
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
    Ok((market, pool_filter(pool_manager, vec![pool_id])))
}

/// The PoolManager's logs for the pools named, by event and pool id.
fn pool_filter(pool_manager: Address, pool_ids: Vec<B256>) -> Filter {
    Filter::new()
        .address(pool_manager)
        .event_signature(vec![
            IPoolManagerState::Initialize::SIGNATURE_HASH,
            IPoolManagerState::Swap::SIGNATURE_HASH,
            IPoolManagerState::ModifyLiquidity::SIGNATURE_HASH,
        ])
        .topic1(pool_ids)
}

/// [`History::markets_tape`](crate::history::History::markets_tape)'s read
/// over a caller-held width search, with up to `in_flight` window requests
/// outstanding per scan.
pub(in crate::history) async fn markets_tape_with<P: Provider>(
    provider: &P,
    markets: &[TapeAddresses],
    from_block: u64,
    to_block: u64,
    pool: PoolLogs,
    widths: &SharedWidths,
    in_flight: usize,
) -> Result<BTreeMap<Address, Tape>> {
    let Some(first) = markets.first() else {
        return Ok(BTreeMap::new());
    };
    let mut addresses = Vec::with_capacity(markets.len() * 2);
    for market in markets {
        // The zero-address checks a single market's read makes.
        tape_filters(*market)?;
        if market.pool_manager != first.pool_manager {
            return Err(ValidationError::InvalidConfig {
                reason: format!(
                    "markets on pool managers {} and {}: one scan reads one",
                    first.pool_manager, market.pool_manager
                ),
            }
            .into());
        }
        if markets
            .iter()
            .filter(|other| other.perp == market.perp)
            .count()
            > 1
        {
            return Err(ValidationError::InvalidConfig {
                reason: format!("market {} named twice: each perp is one tape", market.perp),
            }
            .into());
        }
        addresses.extend([market.perp, market.beacon]);
    }
    addresses.sort_unstable();
    addresses.dedup();
    let mut logs = scan_all(
        provider,
        &Filter::new().address(addresses),
        from_block,
        to_block,
        widths,
        in_flight,
    )
    .await?;
    if pool == PoolLogs::Include {
        let ids = markets.iter().map(|market| market.pool_id).collect();
        let filter = pool_filter(first.pool_manager, ids);
        logs.extend(scan_all(provider, &filter, from_block, to_block, widths, in_flight).await?);
    }
    logs.sort_by_key(|log| (log.block_number, log.log_index));
    let mut decoded: BTreeMap<Address, Vec<(Log, MarketEvent)>> = markets
        .iter()
        .map(|market| (market.perp, Vec::new()))
        .collect();
    for (log, event) in decode_known(logs, widths) {
        let from = log.address();
        for market in markets {
            let pool_log =
                from == market.pool_manager && log.topics().get(1) == Some(&market.pool_id);
            if (from == market.perp || from == market.beacon || pool_log)
                && let Some(rows) = decoded.get_mut(&market.perp)
            {
                rows.push((log.clone(), event));
            }
        }
    }
    let mut tapes = BTreeMap::new();
    for (perp, rows) in decoded {
        tapes.insert(perp, tape_rows(provider, rows).await?);
    }
    Ok(tapes)
}

/// Decodes the logs this vocabulary recognizes, each kept with its log.
///
/// A log of another vocabulary is skipped, which is what a filter on one
/// address returns plenty of. A log of *this* vocabulary that will not
/// decode is counted on the scan's [`ScanStats::undecodable`](crate::history::ScanStats::undecodable) and skipped
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
async fn tape_rows<P: Provider>(provider: &P, decoded: Vec<(Log, MarketEvent)>) -> Result<Tape> {
    let headers = block_timestamps(provider, decoded.iter().map(|(log, _)| log)).await?;
    let rows: Vec<TapeEvent> = decoded
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
        .collect::<Result<_>>()?;
    Ok(Tape::new(rows)?)
}
