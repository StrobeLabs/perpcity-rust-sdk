//! Historical chain reads over block ranges of any length.
//!
//! [`get_logs_chunked`] reads every log that matches a filter across a
//! block range, whatever limits the provider puts on `eth_getLogs`.
//! [`beacon_prints`] and [`latest_beacon_prints`] build a beacon's index
//! series, `(block, timestamp, index)`, on top of it.
//!
//! Providers cap `eth_getLogs` by block span, by result count, or by
//! response size, and each words the rejection differently, so the scan
//! does not parse error messages. When the server answers a range with an
//! error, the scan halves the range and asks again. After an accepted
//! range it doubles the span until the first rejection, then narrows in on
//! the limit between the widest accepted and the narrowest rejected span.
//! A provider with a fixed limit therefore costs a few rejected requests at
//! the start of a scan and none after. A single block that the server
//! still rejects is returned as the error.
//!
//! Failures where the server gave no answer (timeouts, dropped
//! connections, HTTP 429) are returned at once: a smaller range does not
//! fix them, and the caller owns the retry policy
//! ([`PerpCityError::is_transient`](crate::PerpCityError::is_transient)).
//! A scan sends its requests back to back, so against a rate-limited
//! endpoint put the backoff in the transport (alloy's `RetryBackoffLayer`,
//! or [`HftTransport`](crate::HftTransport)'s read retries).
//!
//! The deployed beacons expose no last-update getter (`index()` returns
//! the value alone), so a beacon's newest `IndexUpdated` log is the only
//! record of when it last printed: `latest_beacon_prints(.., 1)` reads it.

use std::collections::{BTreeSet, HashMap};

use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::SolEvent;
use alloy::transports::{RpcError, TransportError, TransportErrorKind};
use futures_util::stream::{self, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};

use crate::constants::{LOG_SCAN_INITIAL_SPAN, LOG_SCAN_MAX_SPAN};
use crate::contracts::IBeacon;
use crate::convert::price_x96_to_f64;
use crate::errors::{ContractError, PerpCityError, Result, ValidationError};
use crate::feeds::events::decode_raw;

/// Concurrent header reads when a provider omits log timestamps.
const HEADER_READ_CONCURRENCY: usize = 4;

/// One index value a beacon published.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexPrint {
    /// Block the print landed in.
    pub block_number: u64,
    /// Position of the print's log in its block.
    pub log_index: u64,
    /// Unix timestamp of the block.
    pub timestamp: u64,
    /// The printed index, Q96 fixed-point, as the beacon emitted it.
    pub index_x96: U256,
}

impl IndexPrint {
    /// The printed index as a float.
    ///
    /// # Errors
    ///
    /// As [`price_x96_to_f64`]: a zero print, or one beyond the safe f64
    /// range.
    pub fn index(&self) -> std::result::Result<f64, ValidationError> {
        price_x96_to_f64(self.index_x96)
    }
}

/// Every index print `beacon` published in blocks `from_block..=to_block`,
/// oldest first.
///
/// # Errors
///
/// [`ValidationError::InvalidConfig`] for the zero address,
/// [`ValidationError::InvalidBlockRange`] if `from_block > to_block`,
/// [`ContractError::BlockUnavailable`] if a print's block header is missing
/// when the provider omits log timestamps, or the RPC error that stopped
/// the scan.
pub async fn beacon_prints<P: Provider>(
    provider: &P,
    beacon: Address,
    from_block: u64,
    to_block: u64,
) -> Result<Vec<IndexPrint>> {
    let filter = index_updated_filter(beacon)?;
    let logs = get_logs_chunked(provider, &filter, from_block, to_block).await?;
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
    let filter = index_updated_filter(beacon)?;
    let mut scan = LogScan::new(provider, &filter, from_block, to_block)?;
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut newest_first = Vec::new();
    let mut held = 0;
    while held < limit
        && let Some(chunk) = scan.next_chunk(End::Newest).await?
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
    let missing: BTreeSet<u64> = logs
        .iter()
        .filter(|log| log.block_timestamp.is_none())
        .filter_map(|log| log.block_number)
        .collect();
    let headers: HashMap<u64, u64> = stream::iter(missing)
        .map(|number| async move {
            let block = provider
                .get_block_by_number(number.into())
                .await?
                .ok_or(ContractError::BlockUnavailable { number })?;
            Ok::<_, PerpCityError>((number, block.header.timestamp))
        })
        .buffered(HEADER_READ_CONCURRENCY)
        .try_collect()
        .await?;
    Ok(logs
        .iter()
        .filter_map(|log| {
            let block_number = log.block_number?;
            let timestamp = log
                .block_timestamp
                .or_else(|| headers.get(&block_number).copied())?;
            let event = decode_raw::<IBeacon::IndexUpdated>(log)?;
            Some(IndexPrint {
                block_number,
                log_index: log.log_index?,
                timestamp,
                index_x96: event.index,
            })
        })
        .collect())
}

/// Every log matching `filter` in blocks `from_block..=to_block`, in chain
/// order.
///
/// The block range of `filter` itself is ignored. See the
/// [module docs](self) for how the range is split into requests.
///
/// # Errors
///
/// [`ValidationError::InvalidBlockRange`] if `from_block > to_block`, or
/// the RPC error for the first request the scan cannot shrink or retry.
pub async fn get_logs_chunked<P: Provider>(
    provider: &P,
    filter: &Filter,
    from_block: u64,
    to_block: u64,
) -> Result<Vec<Log>> {
    let mut scan = LogScan::new(provider, filter, from_block, to_block)?;
    let mut logs = Vec::new();
    while let Some(chunk) = scan.next_chunk(End::Oldest).await? {
        logs.extend(chunk);
    }
    Ok(logs)
}

/// A block range read in adaptive chunks, from either end.
struct LogScan<'a, P> {
    provider: &'a P,
    filter: Filter,
    /// Blocks not yet read, inclusive; `None` once the range is done.
    remaining: Option<(u64, u64)>,
    /// Width of the next request.
    span: u64,
    /// Widest request the server has accepted so far.
    accepted_span: u64,
    /// Narrowest request the server has rejected so far.
    rejected_span: u64,
}

impl<'a, P: Provider> LogScan<'a, P> {
    fn new(
        provider: &'a P,
        filter: &Filter,
        from_block: u64,
        to_block: u64,
    ) -> std::result::Result<Self, ValidationError> {
        if from_block > to_block {
            return Err(ValidationError::InvalidBlockRange {
                from_block,
                to_block,
            });
        }
        Ok(Self {
            provider,
            filter: filter.clone(),
            remaining: Some((from_block, to_block)),
            span: LOG_SCAN_INITIAL_SPAN,
            accepted_span: 0,
            rejected_span: u64::MAX,
        })
    }

    /// The logs of the unread chunk at `end`, or `None` once the range is
    /// read.
    async fn next_chunk(&mut self, end: End) -> Result<Option<Vec<Log>>> {
        let Some((low, high)) = self.remaining else {
            return Ok(None);
        };
        loop {
            let reach = self.span - 1;
            let (from, to) = match end {
                End::Oldest => (low, low.saturating_add(reach).min(high)),
                End::Newest => (high.saturating_sub(reach).max(low), high),
            };
            let filter = self.filter.clone().from_block(from).to_block(to);
            match self.provider.get_logs(&filter).await {
                Ok(logs) => {
                    self.remaining = match end {
                        End::Oldest => (to < high).then(|| (to + 1, high)),
                        End::Newest => (from > low).then(|| (low, from - 1)),
                    };
                    self.accepted(to - from + 1);
                    return Ok(Some(logs));
                }
                Err(error) if to > from && is_range_rejection(&error) => {
                    tracing::debug!(from, to, %error, "eth_getLogs range rejected; narrowing");
                    self.rejected(to - from + 1);
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn accepted(&mut self, width: u64) {
        self.accepted_span = self.accepted_span.max(width);
        if self.accepted_span >= self.rejected_span {
            // The rejection came from a result-count or response-size cap
            // in a denser stretch of the range; the span limit is unknown
            // again.
            self.rejected_span = u64::MAX;
        }
        self.span = if self.rejected_span == u64::MAX {
            self.span.saturating_mul(2).min(LOG_SCAN_MAX_SPAN)
        } else {
            self.probe_span()
        };
    }

    fn rejected(&mut self, width: u64) {
        self.rejected_span = self.rejected_span.min(width);
        self.span = if self.accepted_span == 0 {
            width / 2
        } else {
            self.probe_span()
        };
    }

    /// The midpoint between the widest accepted and the narrowest rejected
    /// span, or the accepted span once the two are within an eighth of it.
    fn probe_span(&self) -> u64 {
        let accepted = self.accepted_span.min(self.rejected_span - 1);
        let gap = self.rejected_span - accepted;
        if gap <= (accepted / 8).max(1) {
            accepted
        } else {
            accepted + gap / 2
        }
    }
}

#[derive(Clone, Copy)]
enum End {
    Oldest,
    Newest,
}

/// Whether the server answered the request with a rejection that a
/// narrower range may avoid: a JSON-RPC error, or an HTTP error status
/// other than 429 (a gateway timing out a wide scan answers 504).
fn is_range_rejection(error: &TransportError) -> bool {
    match error {
        RpcError::ErrorResp(_) => true,
        RpcError::Transport(TransportErrorKind::HttpError(http)) => http.status != 429,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, B256, Bytes, U256, address, b256, bytes};

    use super::*;
    use crate::constants::Q96;
    use crate::test_support::{FakeNode, Mode, mined_log, timestamp_of};

    const EMITTER: Address = Address::repeat_byte(0xAA);
    const TOPIC: B256 = B256::repeat_byte(0x11);

    /// One log every `step` blocks in `0..=last`.
    fn logs_every(step: u64, last: u64) -> Vec<Log> {
        (0..=last)
            .step_by(step as usize)
            .map(|block| mined_log(EMITTER, TOPIC, Bytes::new(), block, 0))
            .collect()
    }

    fn filter() -> Filter {
        Filter::new().address(EMITTER).event_signature(TOPIC)
    }

    fn blocks(logs: &[Log]) -> Vec<u64> {
        logs.iter().map(|log| log.block_number.unwrap()).collect()
    }

    /// The accepted requests tile `from..=to` with no gap and no overlap.
    fn assert_tiles(accepted: &[(u64, u64)], from: u64, to: u64) {
        let mut sorted = accepted.to_vec();
        sorted.sort_unstable();
        let mut next = from;
        for &(start, end) in &sorted {
            assert_eq!(start, next, "gap or overlap before {start} in {sorted:?}");
            next = end + 1;
        }
        assert_eq!(next, to + 1, "range not covered to its end: {sorted:?}");
    }

    #[tokio::test]
    async fn an_unlimited_provider_is_read_in_growing_chunks() {
        let node = FakeNode::new(logs_every(1_000, 350_000), u64::MAX);
        let logs = get_logs_chunked(&node.provider(), &filter(), 0, 350_000)
            .await
            .unwrap();
        assert_eq!(
            blocks(&logs),
            (0..=350_000).step_by(1_000).collect::<Vec<_>>()
        );
        assert_eq!(
            node.requests(),
            vec![(0, 99_999), (100_000, 299_999), (300_000, 350_000)]
        );
    }

    #[tokio::test]
    async fn a_capped_provider_is_learned_once_and_the_range_read_exactly() {
        let cap = 10_000;
        let node = FakeNode::new(logs_every(777, 120_000), cap);
        let logs = get_logs_chunked(&node.provider(), &filter(), 0, 120_000)
            .await
            .unwrap();
        assert_eq!(
            blocks(&logs),
            (0..=120_000).step_by(777).collect::<Vec<_>>()
        );

        let requests = node.requests();
        let (accepted, rejected): (Vec<_>, Vec<_>) =
            requests.iter().partition(|(from, to)| to - from < cap);
        assert_tiles(&accepted, 0, 120_000);
        // Four halvings from 100k to an accepted 6,250, then two probes
        // between the accepted and rejected widths; none after that.
        assert_eq!(rejected.len(), 6, "rejected requests: {requests:?}");
    }

    #[tokio::test]
    async fn newest_first_reads_the_same_range_from_the_top() {
        let node = FakeNode::new(logs_every(500, 60_000), 7_000);
        let provider = node.provider();
        let mut scan = LogScan::new(&provider, &filter(), 1_000, 60_000).unwrap();
        let mut seen = Vec::new();
        while let Some(chunk) = scan.next_chunk(End::Newest).await.unwrap() {
            let chunk = blocks(&chunk);
            if let (Some(last), Some(first)) = (seen.last(), chunk.first()) {
                assert!(first < last, "chunks must arrive newest first");
            }
            seen.extend(chunk.into_iter().rev());
        }
        seen.reverse();
        assert_eq!(seen, (1_000..=60_000).step_by(500).collect::<Vec<_>>());
        let accepted: Vec<_> = node
            .requests()
            .into_iter()
            .filter(|(from, to)| to - from < 7_000)
            .collect();
        assert_tiles(&accepted, 1_000, 60_000);
    }

    const BEACON: Address = Address::repeat_byte(0xBE);

    fn print_log(block: u64, index: u64, value: U256, timestamp: Option<u64>) -> Log {
        let mut log = mined_log(
            BEACON,
            IBeacon::IndexUpdated::SIGNATURE_HASH,
            value.to_be_bytes_vec().into(),
            block,
            index,
        );
        log.block_timestamp = timestamp;
        log
    }

    #[tokio::test]
    async fn prints_take_log_timestamps_and_read_each_missing_header_once() {
        let logs = vec![
            print_log(10, 0, Q96, Some(42)),
            print_log(20, 1, Q96 * U256::from(2), None),
            print_log(20, 4, Q96 * U256::from(3), None),
            print_log(30, 0, Q96 * U256::from(4), None),
        ];
        let node = FakeNode::new(logs, u64::MAX);
        let prints = beacon_prints(&node.provider(), BEACON, 0, 100)
            .await
            .unwrap();

        let rows: Vec<_> = prints
            .iter()
            .map(|p| (p.block_number, p.log_index, p.timestamp, p.index().unwrap()))
            .collect();
        assert_eq!(
            rows,
            vec![
                (10, 0, 42, 1.0),
                (20, 1, timestamp_of(20), 2.0),
                (20, 4, timestamp_of(20), 3.0),
                (30, 0, timestamp_of(30), 4.0),
            ]
        );
        assert_eq!(node.header_reads(), vec![20, 30]);
    }

    /// A real `IndexUpdated` log from the Arbitrum One beacon
    /// 0x0a33ea45fe9011029641ef63ce8e1c94a8a29990, as the node returned it
    /// (tx 0x598da8ad…, which carries `blockTimestamp`).
    #[tokio::test]
    async fn a_mainnet_print_decodes_to_its_block_and_value() {
        let beacon = address!("0a33ea45fe9011029641ef63ce8e1c94a8a29990");
        let mut log = mined_log(
            beacon,
            b256!("acfc085c9be45d2b3f9e5c09a19d4a95749cc16939519c13e090de3a4cb192c6"),
            bytes!("0000000000000000000000000000000000000010edbd439f2076368fba399c29"),
            0x1e2c_c005,
            3,
        );
        log.block_timestamp = Some(0x6aac_7525);
        let node = FakeNode::new(vec![log], u64::MAX);
        let prints = beacon_prints(&node.provider(), beacon, 0x1e2c_0000, 0x1e2d_0000)
            .await
            .unwrap();
        assert_eq!(prints.len(), 1);
        let print = prints[0];
        assert_eq!(
            (print.block_number, print.log_index, print.timestamp),
            (506_249_221, 3, 1_789_687_077)
        );
        assert!((print.index().unwrap() - 16.928669).abs() < 1e-6);
        assert!(node.header_reads().is_empty());
    }

    #[tokio::test]
    async fn latest_prints_read_back_only_as_far_as_the_limit_needs() {
        let logs = (0..=600_000)
            .step_by(500)
            .map(|block| print_log(block, 0, Q96 * U256::from(block + 1), Some(block)))
            .collect();
        let node = FakeNode::new(logs, 7_000);
        let prints = latest_beacon_prints(&node.provider(), BEACON, 0, 600_000, 5)
            .await
            .unwrap();
        let blocks: Vec<u64> = prints.iter().map(|p| p.block_number).collect();
        assert_eq!(blocks, vec![598_000, 598_500, 599_000, 599_500, 600_000]);
        let served: Vec<_> = node
            .requests()
            .into_iter()
            .filter(|(from, to)| to - from < 7_000)
            .collect();
        assert_eq!(
            served.len(),
            1,
            "one served chunk holds five prints: {served:?}"
        );
        assert!(
            served[0].0 > 590_000,
            "read further back than needed: {served:?}"
        );
    }

    #[tokio::test]
    async fn a_zero_limit_or_zero_beacon_reads_nothing() {
        let node = FakeNode::new(vec![print_log(1, 0, Q96, Some(1))], u64::MAX);
        let provider = node.provider();
        assert!(
            latest_beacon_prints(&provider, BEACON, 0, 10, 0)
                .await
                .unwrap()
                .is_empty()
        );
        let error = beacon_prints(&provider, Address::ZERO, 0, 10)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PerpCityError::Validation(ValidationError::InvalidConfig { .. })
        ));
        assert!(node.requests().is_empty());
    }

    #[test]
    fn a_zero_print_has_no_float_index() {
        let print = IndexPrint {
            block_number: 1,
            log_index: 0,
            timestamp: 1,
            index_x96: U256::ZERO,
        };
        assert!(print.index().is_err());
    }

    #[tokio::test]
    async fn a_single_rejected_block_is_the_error() {
        let node = FakeNode::new(Vec::new(), 0);
        let error = get_logs_chunked(&node.provider(), &filter(), 0, 3)
            .await
            .unwrap_err();
        assert!(matches!(error, PerpCityError::Rpc(RpcError::ErrorResp(_))));
        assert_eq!(node.requests().last(), Some(&(0, 0)));
    }

    #[tokio::test]
    async fn a_rate_limit_is_returned_without_shrinking() {
        let node = FakeNode::new(Vec::new(), u64::MAX).with_mode(Mode::RateLimited);
        let error = get_logs_chunked(&node.provider(), &filter(), 0, 500_000)
            .await
            .unwrap_err();
        assert!(error.is_transient());
        assert_eq!(node.requests(), vec![(0, 99_999)]);
    }

    #[tokio::test]
    async fn a_reversed_range_is_rejected_before_any_request() {
        let node = FakeNode::new(Vec::new(), u64::MAX);
        let error = get_logs_chunked(&node.provider(), &filter(), 5, 4)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PerpCityError::Validation(ValidationError::InvalidBlockRange {
                from_block: 5,
                to_block: 4
            })
        ));
        assert!(node.requests().is_empty());
    }
}
