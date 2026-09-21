//! Historical chain reads over block ranges of any length.
//!
//! [`get_logs_chunked`] reads every log that matches a filter across a
//! block range, whatever limits the provider puts on `eth_getLogs`.
//! [`beacon_prints`] and [`latest_beacon_prints`] build a beacon's index
//! series, `(block, timestamp, index)`, on top of it, and
//! [`token_transfers`] reads an ERC-20's `Transfer` events between address
//! sets (for example, every USDC transfer between a treasury and its
//! wallets).
//!
//! Providers cap `eth_getLogs` by block span, by result count, or by
//! response size, and each words the rejection differently, so the scan
//! does not parse error messages. When the server answers a range with an
//! error, the scan halves the range and asks again. After an accepted
//! range it doubles the span until the first rejection, then narrows in on
//! the limit between the widest accepted and the narrowest rejected span.
//! A rejection of a span the server accepted before shows a result-count
//! or size cap in a denser stretch; the scan then drops what it learned
//! and halves again. After a run of accepted requests it tests the
//! rejected span once more, so it widens again past a dense stretch; each
//! time the limit holds, the run before the next test doubles. A provider
//! with a fixed span limit therefore costs a few rejected requests at the
//! start of a scan and a logarithmic number after. A single block that the
//! server still rejects is returned as [`ContractError::LogsRejected`].
//!
//! Failures that a smaller range does not fix are returned at once: no
//! answer (timeouts, dropped connections), a rate limit (HTTP 429 or 503,
//! or a JSON-RPC error that alloy's retry rules call one), a method or
//! parse error, and HTTP 401 or 403. A refusal (method, parse, auth) is
//! [`ContractError::LogsRejected`], which is not transient; the rest keep
//! the transport error, which is. The caller owns the retry policy
//! ([`PerpCityError::is_transient`](crate::PerpCityError::is_transient)).
//! A scan sends its requests back to back, so against a rate-limited
//! endpoint put the backoff in the transport (alloy's `RetryBackoffLayer`,
//! or [`HftTransport`](crate::HftTransport)'s read retries).
//!
//! The deployed beacons expose no last-update getter (`index()` returns
//! the value alone), so a beacon's newest `IndexUpdated` log is the only
//! record of when it last printed: `latest_beacon_prints(.., 1)` reads it.

use std::collections::{BTreeSet, HashMap};

use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::json_rpc::ErrorPayload;
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::SolEvent;
use alloy::transports::{RpcError, TransportError, TransportErrorKind};
use futures_util::stream::{self, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};

use crate::constants::{LOG_FILTER_MAX_TOPIC_VALUES, LOG_SCAN_INITIAL_SPAN, LOG_SCAN_MAX_SPAN};
use crate::contracts::{IBeacon, IERC20};
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
/// when the provider omits log timestamps,
/// [`ValidationError::DecodeFailed`] for an `IndexUpdated` log that does
/// not decode, or an error from [`get_logs_chunked`].
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
        .buffer_unordered(HEADER_READ_CONCURRENCY)
        .try_collect()
        .await?;
    logs.iter()
        .map(|log| {
            let decoded = log
                .block_number
                .zip(log.log_index)
                .and_then(|(block, index)| {
                    let event = decode_raw::<IBeacon::IndexUpdated>(log)?;
                    Some((block, index, event.index))
                });
            let (block_number, log_index, index_x96) =
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
                index_x96,
            })
        })
        .collect()
}

/// One ERC-20 `Transfer` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenTransfer {
    /// Block the transfer landed in.
    pub block_number: u64,
    /// Position of the transfer's log in its block.
    pub log_index: u64,
    /// Transaction that emitted the transfer.
    pub tx_hash: B256,
    /// Sender.
    pub from: Address,
    /// Recipient.
    pub to: Address,
    /// Amount in the token's smallest unit.
    pub value: U256,
}

/// Every `Transfer` of `token` in blocks `from_block..=to_block` whose
/// sender is in `senders` and whose recipient is in `recipients`, in chain
/// order.
///
/// `None` matches any address and `Some(&[])` matches none, so a set that
/// comes out empty reads nothing rather than every transfer. At least one
/// side must be `Some`: an unfiltered query reads every transfer the token
/// ever made. The sets go to the node as topic filters, so the scan
/// returns only matching logs. Geth-based nodes (Arbitrum Nitro among
/// them) cap a topic filter at 1,000 values, so a larger set is split into
/// several scans of the range, one per pair of sender and recipient
/// chunks, run one after another.
///
/// # Errors
///
/// [`ValidationError::InvalidConfig`] for the zero token or two `None`
/// sets, [`ValidationError::InvalidBlockRange`] if
/// `from_block > to_block`, [`ValidationError::DecodeFailed`] for a log
/// that does not decode as an ERC-20 `Transfer` (for example, an ERC-721
/// `Transfer`, which shares the topic but indexes its third argument), or
/// an error from [`get_logs_chunked`]. Any error fails the whole call.
pub async fn token_transfers<P: Provider>(
    provider: &P,
    token: Address,
    senders: Option<&[Address]>,
    recipients: Option<&[Address]>,
    from_block: u64,
    to_block: u64,
) -> Result<Vec<TokenTransfer>> {
    let filters = transfer_filters(token, senders, recipients, from_block, to_block)?;
    let mut transfers = Vec::new();
    for filter in &filters {
        for log in get_logs_chunked(provider, filter, from_block, to_block).await? {
            transfers.push(decode_transfer(&log)?);
        }
    }
    if filters.len() > 1 {
        transfers.sort_unstable_by_key(|t| (t.block_number, t.log_index));
    }
    Ok(transfers)
}

/// The filters for [`token_transfers`], one per pair of sender and
/// recipient chunks; none when an empty set means nothing can match.
fn transfer_filters(
    token: Address,
    senders: Option<&[Address]>,
    recipients: Option<&[Address]>,
    from_block: u64,
    to_block: u64,
) -> std::result::Result<Vec<Filter>, ValidationError> {
    if token.is_zero() {
        return Err(ValidationError::InvalidConfig {
            reason: "token address is zero".into(),
        });
    }
    if senders.is_none() && recipients.is_none() {
        return Err(ValidationError::InvalidConfig {
            reason: "a transfer query needs a sender or a recipient set".into(),
        });
    }
    check_block_range(from_block, to_block)?;
    let base = Filter::new()
        .address(token)
        .event_signature(IERC20::Transfer::SIGNATURE_HASH);
    let recipient_chunks = topic_chunks(recipients);
    let mut filters = Vec::new();
    for sender_chunk in topic_chunks(senders) {
        for recipient_chunk in &recipient_chunks {
            let mut filter = base.clone();
            if let Some(chunk) = &sender_chunk {
                filter = filter.topic1(chunk.clone());
            }
            if let Some(chunk) = recipient_chunk {
                filter = filter.topic2(chunk.clone());
            }
            filters.push(filter);
        }
    }
    Ok(filters)
}

/// An address set as topic-filter values: `[None]` (any address) for no
/// set, otherwise the distinct addresses in chunks a node accepts, so the
/// chunks never overlap and an empty set has none.
fn topic_chunks(set: Option<&[Address]>) -> Vec<Option<Vec<B256>>> {
    let Some(set) = set else {
        return vec![None];
    };
    let words: Vec<B256> = set
        .iter()
        .map(|address| address.into_word())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    words
        .chunks(LOG_FILTER_MAX_TOPIC_VALUES)
        .map(|chunk| Some(chunk.to_vec()))
        .collect()
}

fn decode_transfer(log: &Log) -> Result<TokenTransfer> {
    let decoded = log
        .block_number
        .zip(log.log_index)
        .zip(log.transaction_hash)
        .and_then(|((block, index), tx_hash)| {
            let event = decode_raw::<IERC20::Transfer>(log)?;
            Some(TokenTransfer {
                block_number: block,
                log_index: index,
                tx_hash,
                from: event.from,
                to: event.to,
                value: event.value,
            })
        });
    decoded.ok_or_else(|| {
        ValidationError::DecodeFailed {
            context: format!(
                "Transfer log from {} in tx {:?}",
                log.address(),
                log.transaction_hash
            ),
        }
        .into()
    })
}

/// Every log matching `filter` in blocks `from_block..=to_block`, in chain
/// order.
///
/// The block range of `filter` itself is ignored. See the
/// [module docs](self) for how the range is split into requests.
///
/// # Errors
///
/// [`ValidationError::InvalidBlockRange`] if `from_block > to_block`,
/// [`ContractError::LogsRejected`] for a request the server refuses at any
/// width, or the transport error for a request it did not answer.
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

fn check_block_range(from_block: u64, to_block: u64) -> std::result::Result<(), ValidationError> {
    if from_block > to_block {
        return Err(ValidationError::InvalidBlockRange {
            from_block,
            to_block,
        });
    }
    Ok(())
}

/// A block range read in adaptive chunks, from either end.
struct LogScan<'a, P> {
    provider: &'a P,
    filter: Filter,
    /// Blocks not yet read, inclusive; `None` once the range is done.
    remaining: Option<(u64, u64)>,
    widths: WidthSearch,
}

impl<'a, P: Provider> LogScan<'a, P> {
    fn new(
        provider: &'a P,
        filter: &Filter,
        from_block: u64,
        to_block: u64,
    ) -> std::result::Result<Self, ValidationError> {
        check_block_range(from_block, to_block)?;
        Ok(Self {
            provider,
            filter: filter.clone(),
            remaining: Some((from_block, to_block)),
            widths: WidthSearch::new(),
        })
    }

    /// The logs of the unread chunk at `end`, or `None` once the range is
    /// read.
    async fn next_chunk(&mut self, end: End) -> Result<Option<Vec<Log>>> {
        let Some((low, high)) = self.remaining else {
            return Ok(None);
        };
        loop {
            let reach = self.widths.next - 1;
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
                    self.widths.accepted(to - from + 1);
                    return Ok(Some(logs));
                }
                Err(error) => match classify(&error) {
                    Failure::Range if to > from => {
                        tracing::debug!(from, to, %error, "eth_getLogs range rejected; narrowing");
                        self.widths.rejected(to - from + 1);
                    }
                    Failure::Range | Failure::Refused => {
                        return Err(ContractError::LogsRejected {
                            from_block: from,
                            to_block: to,
                            source: error,
                        }
                        .into());
                    }
                    Failure::Unanswered => return Err(error.into()),
                },
            }
        }
    }
}

/// Accepted requests in a row after which the scan tests a width it saw
/// rejected again, and the cap on that interval as it doubles.
const RETEST_AFTER: (u32, u32) = (16, 1_024);

/// The width of the next `eth_getLogs` request, learned from the server's
/// answers.
///
/// Holds `accepted < rejected` whenever both are known: an answer that
/// breaks it shows the cap is on result count or response size, not on
/// span, so the contradicted bound is dropped.
#[derive(Debug)]
struct WidthSearch {
    /// Width of the next request; at least 1.
    next: u64,
    /// Widest width accepted since the last contradiction.
    accepted: Option<u64>,
    /// Narrowest width rejected since the last retest.
    rejected: Option<u64>,
    /// Accepted requests since the last rejection.
    accepted_run: u32,
    /// Accepted run length that triggers a retest of `rejected`.
    retest_after: u32,
}

impl WidthSearch {
    fn new() -> Self {
        Self {
            next: LOG_SCAN_INITIAL_SPAN,
            accepted: None,
            rejected: None,
            accepted_run: 0,
            retest_after: RETEST_AFTER.0,
        }
    }

    fn accepted(&mut self, width: u64) {
        let accepted = self.accepted.map_or(width, |a| a.max(width));
        self.accepted = Some(accepted);
        self.accepted_run += 1;
        match self.rejected {
            Some(rejected) if accepted >= rejected => {
                // A denser stretch caused the rejection; this one is sparser.
                self.rejected = None;
                self.retest_after = RETEST_AFTER.0;
            }
            Some(_) if self.accepted_run >= self.retest_after => {
                // A result or size cap may have moved since the rejection;
                // test it again, less often each time it holds.
                self.rejected = None;
                self.accepted_run = 0;
                self.retest_after = (self.retest_after * 2).min(RETEST_AFTER.1);
            }
            _ => {}
        }
        self.next = match self.rejected {
            None => self
                .next
                .max(width)
                .saturating_mul(2)
                .min(LOG_SCAN_MAX_SPAN),
            Some(rejected) => probe(accepted, rejected),
        };
    }

    fn rejected(&mut self, width: u64) {
        self.accepted_run = 0;
        if self.accepted.is_some_and(|accepted| width <= accepted) {
            // A width accepted elsewhere: the cap is on result count or
            // response size, and this stretch is denser.
            self.accepted = None;
            self.retest_after = RETEST_AFTER.0;
        }
        let rejected = self.rejected.map_or(width, |r| r.min(width));
        self.rejected = Some(rejected);
        self.next = match self.accepted {
            None => width / 2,
            Some(accepted) => probe(accepted, rejected),
        };
    }
}

/// The midpoint of `accepted..rejected`, or `accepted` once the gap is
/// within an eighth of it.
fn probe(accepted: u64, rejected: u64) -> u64 {
    let gap = rejected - accepted;
    if gap <= (accepted / 8).max(1) {
        accepted
    } else {
        accepted + gap / 2
    }
}

#[derive(Clone, Copy)]
enum End {
    Oldest,
    Newest,
}

/// What a failed `eth_getLogs` request tells the scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    /// The server refused the range; a narrower one may pass.
    Range,
    /// The server refused the request for a reason no range fixes.
    Refused,
    /// The server gave no answer, or asked the client to slow down.
    Unanswered,
}

/// Classifies a failed request.
///
/// Providers word range rejections differently and reuse codes for them
/// (Infura's `-32005` is both "more than 10000 results" and a rate limit),
/// so any server error is a range rejection unless it is a rate limit, a
/// method or parse error, or an auth or availability status.
fn classify(error: &TransportError) -> Failure {
    match error {
        RpcError::ErrorResp(payload) if is_rate_limit(payload) => Failure::Unanswered,
        RpcError::ErrorResp(payload) if matches!(payload.code, -32_601 | -32_700) => {
            Failure::Refused
        }
        RpcError::ErrorResp(_) => Failure::Range,
        RpcError::Transport(TransportErrorKind::HttpError(http)) => match http.status {
            429 | 503 => Failure::Unanswered,
            401 | 403 => Failure::Refused,
            _ => Failure::Range,
        },
        _ => Failure::Unanswered,
    }
}

/// Whether a JSON-RPC error is a rate limit, per alloy's retry rules, with
/// `-32005` a rate limit only when its message says so.
fn is_rate_limit(payload: &ErrorPayload) -> bool {
    if payload.code != -32_005 {
        return payload.is_retry_err();
    }
    ErrorPayload::<()> {
        code: 0,
        message: payload.message.clone(),
        data: None,
    }
    .is_retry_err()
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{
        Address, B256, Bytes, Log as PrimitiveLog, LogData, U256, address, b256, bytes,
    };

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
    async fn a_dense_stretch_under_a_result_cap_is_narrowed_by_halving() {
        let sparse = logs_every(1_000, 400_000)
            .into_iter()
            .filter(|log| !(300_000..=310_000).contains(&log.block_number.unwrap()));
        let dense =
            (300_000..=310_000).map(|block| mined_log(EMITTER, TOPIC, Bytes::new(), block, 0));
        let mut logs: Vec<Log> = sparse.chain(dense).collect();
        logs.sort_by_key(|log| log.block_number);
        let expected = blocks(&logs);
        let node = FakeNode::new(logs, u64::MAX).with_max_results(1_000);

        let read = get_logs_chunked(&node.provider(), &filter(), 0, 400_000)
            .await
            .unwrap();
        assert_eq!(blocks(&read), expected);
        let requests = node.requests();
        // Eleven chunks are the least that fit the dense stretch under the
        // cap; the rest is the halving into it and the regrowth after it.
        assert!(
            requests.len() < 40,
            "{} requests to read one dense stretch: {requests:?}",
            requests.len()
        );
    }

    #[tokio::test]
    async fn a_long_capped_scan_retests_the_limit_ever_less_often() {
        let cap = 10_000;
        let node = FakeNode::new(logs_every(5_000, 3_000_000), cap);
        let logs = get_logs_chunked(&node.provider(), &filter(), 0, 3_000_000)
            .await
            .unwrap();
        assert_eq!(logs.len(), 601);
        let requests = node.requests();
        let (accepted, rejected): (Vec<_>, Vec<_>) =
            requests.iter().partition(|(from, to)| to - from < cap);
        assert_tiles(&accepted, 0, 3_000_000);
        // 300 chunks at the cap is the floor. Six rejections learn the
        // limit; each retest (after 16, 32, 64, 128 and 256 accepted
        // chunks) costs about three more.
        assert!(accepted.len() <= 330, "{} accepted", accepted.len());
        assert!(rejected.len() <= 24, "rejected requests: {rejected:?}");
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

    #[tokio::test]
    async fn an_undecodable_print_is_an_error_not_a_gap() {
        let mut short = print_log(20, 0, Q96, Some(20));
        short.inner.data = LogData::new_unchecked(short.topics().to_vec(), Bytes::new());
        let node = FakeNode::new(vec![print_log(10, 0, Q96, Some(10)), short], u64::MAX);
        let error = beacon_prints(&node.provider(), BEACON, 0, 100)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PerpCityError::Validation(ValidationError::DecodeFailed { .. })
        ));
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

    const USDC: Address = address!("af88d065e77c8cc2239327c5edb3a432268e5831");
    const TREASURY: Address = Address::repeat_byte(0x01);
    const WALLET_A: Address = Address::repeat_byte(0x0A);
    const WALLET_B: Address = Address::repeat_byte(0x0B);
    const OUTSIDER: Address = Address::repeat_byte(0x0C);

    fn transfer_log(from: Address, to: Address, value: u64, block: u64) -> Log {
        let mut log = mined_log(USDC, B256::ZERO, Bytes::new(), block, 0);
        log.inner.data = LogData::new_unchecked(
            vec![
                IERC20::Transfer::SIGNATURE_HASH,
                from.into_word(),
                to.into_word(),
            ],
            U256::from(value).to_be_bytes_vec().into(),
        );
        log
    }

    fn flows(transfers: &[TokenTransfer]) -> Vec<(Address, Address, U256, u64)> {
        transfers
            .iter()
            .map(|t| (t.from, t.to, t.value, t.block_number))
            .collect()
    }

    /// A real Arbitrum One USDC transfer of 89.999998 USDC, as the node
    /// returned it in the receipt of tx 0xa55d3cc1…e26e.
    #[tokio::test]
    async fn a_mainnet_usdc_transfer_decodes_to_its_parties_and_value() {
        let sender = address!("c3da549ee508386a12f3908d5bf3060fd04b89f5");
        let recipient = address!("e4fb292b59e3d2cdcc16a332035058f9796b5786");
        let tx_hash = b256!("a55d3cc1c657e72ac6d34f47fc20ad1ac7dce3de2c497b3bcf1d559057e6e26e");
        let log = Log {
            inner: PrimitiveLog {
                address: USDC,
                data: LogData::new_unchecked(
                    vec![
                        b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"),
                        b256!("000000000000000000000000c3da549ee508386a12f3908d5bf3060fd04b89f5"),
                        b256!("000000000000000000000000e4fb292b59e3d2cdcc16a332035058f9796b5786"),
                    ],
                    bytes!("00000000000000000000000000000000000000000000000000000000055d4a7e"),
                ),
            },
            block_hash: Some(b256!(
                "5414fbeaa040956c8fce9279e1253bbf0772f9a472030ebd72f9c04fb5d90071"
            )),
            block_number: Some(0x1e40_09af),
            block_timestamp: Some(0x6ab1_68d8),
            transaction_hash: Some(tx_hash),
            transaction_index: Some(1),
            log_index: Some(0),
            removed: false,
        };
        let node = FakeNode::new(vec![log], u64::MAX);
        let transfers = token_transfers(
            &node.provider(),
            USDC,
            Some(&[sender]),
            Some(&[recipient]),
            0x1e40_0000,
            0x1e41_0000,
        )
        .await
        .unwrap();
        assert_eq!(
            transfers,
            vec![TokenTransfer {
                block_number: 507_513_263,
                log_index: 0,
                tx_hash,
                from: sender,
                to: recipient,
                value: U256::from(89_999_998u64),
            }]
        );
    }

    #[tokio::test]
    async fn transfers_are_filtered_by_both_sets_and_none_is_any() {
        let logs = vec![
            transfer_log(TREASURY, WALLET_A, 100, 10),
            transfer_log(TREASURY, OUTSIDER, 7, 11),
            transfer_log(WALLET_B, TREASURY, 40, 12),
            transfer_log(OUTSIDER, TREASURY, 5, 13),
            transfer_log(WALLET_A, WALLET_B, 1, 14),
        ];
        let node = FakeNode::new(logs, u64::MAX);
        let provider = node.provider();
        let wallets = [WALLET_A, WALLET_B];

        let out = token_transfers(&provider, USDC, Some(&[TREASURY]), Some(&wallets), 0, 100)
            .await
            .unwrap();
        assert_eq!(flows(&out), vec![(TREASURY, WALLET_A, U256::from(100), 10)]);

        let back = token_transfers(&provider, USDC, Some(&wallets), Some(&[TREASURY]), 0, 100)
            .await
            .unwrap();
        assert_eq!(flows(&back), vec![(WALLET_B, TREASURY, U256::from(40), 12)]);

        let any_recipient = token_transfers(&provider, USDC, Some(&[TREASURY]), None, 0, 100)
            .await
            .unwrap();
        assert_eq!(
            flows(&any_recipient),
            vec![
                (TREASURY, WALLET_A, U256::from(100), 10),
                (TREASURY, OUTSIDER, U256::from(7), 11),
            ]
        );
    }

    #[tokio::test]
    async fn an_unfiltered_or_zero_token_query_is_refused_before_any_request() {
        let node = FakeNode::new(Vec::new(), u64::MAX);
        let provider = node.provider();
        for (token, senders) in [(USDC, None), (Address::ZERO, Some(&[TREASURY][..]))] {
            let error = token_transfers(&provider, token, senders, None, 0, 100)
                .await
                .unwrap_err();
            assert!(
                matches!(
                    error,
                    PerpCityError::Validation(ValidationError::InvalidConfig { .. })
                ),
                "{error:?}"
            );
        }
        assert!(node.requests().is_empty());
    }

    #[tokio::test]
    async fn an_empty_set_matches_nothing_without_a_request() {
        let node = FakeNode::new(vec![transfer_log(TREASURY, WALLET_A, 100, 10)], u64::MAX);
        let provider = node.provider();
        for (senders, recipients) in [
            (Some(&[][..]), None),
            (None, Some(&[][..])),
            (Some(&[TREASURY][..]), Some(&[][..])),
        ] {
            let out = token_transfers(&provider, USDC, senders, recipients, 0, 100)
                .await
                .unwrap();
            assert!(out.is_empty(), "{out:?}");
        }
        assert!(node.requests().is_empty());

        let error = token_transfers(&provider, USDC, Some(&[]), None, 100, 0)
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                PerpCityError::Validation(ValidationError::InvalidBlockRange { .. })
            ),
            "{error:?}"
        );
    }

    /// 1,500 senders exceed a node's 1,000-value topic limit, so the scan
    /// is split in two; the halves merge back into chain order, and a
    /// sender listed twice is read once.
    #[tokio::test]
    async fn a_set_over_the_topic_limit_is_split_and_merged_in_chain_order() {
        let senders: Vec<Address> = (1..=1_500u64)
            .map(|i| Address::from_word(U256::from(i).into()))
            .collect();
        let (low, high) = (senders[2], senders[1_399]);
        let node = FakeNode::new(
            vec![
                transfer_log(high, WALLET_A, 1, 5),
                transfer_log(low, WALLET_A, 2, 9),
                transfer_log(high, WALLET_A, 3, 12),
                transfer_log(low, WALLET_B, 4, 13),
            ],
            u64::MAX,
        );
        let mut listed = senders.clone();
        listed.push(low);
        let out = token_transfers(
            &node.provider(),
            USDC,
            Some(&listed),
            Some(&[WALLET_A]),
            0,
            100,
        )
        .await
        .unwrap();
        assert_eq!(
            flows(&out),
            vec![
                (high, WALLET_A, U256::from(1), 5),
                (low, WALLET_A, U256::from(2), 9),
                (high, WALLET_A, U256::from(3), 12),
            ]
        );
        assert_eq!(node.requests(), vec![(0, 100), (0, 100)]);
    }

    /// An ERC-721 `Transfer` shares the topic but indexes the token id, so
    /// it has no data to decode as a value.
    #[tokio::test]
    async fn a_transfer_that_does_not_decode_is_an_error_not_a_gap() {
        let mut nft = transfer_log(TREASURY, WALLET_A, 0, 10);
        nft.inner.data = LogData::new_unchecked(
            vec![
                IERC20::Transfer::SIGNATURE_HASH,
                TREASURY.into_word(),
                WALLET_A.into_word(),
                B256::with_last_byte(9),
            ],
            Bytes::new(),
        );
        let node = FakeNode::new(vec![nft], u64::MAX);
        let error = token_transfers(&node.provider(), USDC, Some(&[TREASURY]), None, 0, 100)
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                PerpCityError::Validation(ValidationError::DecodeFailed { .. })
            ),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_single_rejected_block_is_the_error() {
        let node = FakeNode::new(Vec::new(), 0);
        let error = get_logs_chunked(&node.provider(), &filter(), 0, 3)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PerpCityError::Contract(ContractError::LogsRejected {
                from_block: 0,
                to_block: 0,
                ..
            })
        ));
        assert!(!error.is_transient());
        assert_eq!(node.requests().last(), Some(&(0, 0)));
    }

    #[tokio::test]
    async fn failures_no_narrower_range_fixes_are_returned_at_once() {
        let unanswered = [
            Mode::Http(429),
            Mode::Http(503),
            Mode::RpcError(
                429,
                "Your app has exceeded its compute units per second capacity",
            ),
            Mode::RpcError(
                -32_005,
                "daily request count exceeded, request rate limited",
            ),
        ];
        let refused = [
            Mode::Http(401),
            Mode::Http(403),
            Mode::RpcError(-32_601, "the method eth_getLogs does not exist"),
            Mode::RpcError(-32_700, "parse error"),
        ];
        let cases = unanswered
            .into_iter()
            .map(|mode| (mode, true))
            .chain(refused.into_iter().map(|mode| (mode, false)));
        for (mode, transient) in cases {
            let node = FakeNode::new(Vec::new(), u64::MAX).with_mode(mode);
            let error = get_logs_chunked(&node.provider(), &filter(), 0, 500_000)
                .await
                .unwrap_err();
            assert_eq!(error.is_transient(), transient, "{mode:?}: {error}");
            assert_eq!(node.requests(), vec![(0, 99_999)], "{mode:?}");
        }
    }

    #[tokio::test]
    async fn range_rejections_narrow_to_a_single_block() {
        let modes = [
            Mode::Http(504),
            Mode::RpcError(-32_005, "query returned more than 10000 results"),
            Mode::RpcError(-32_602, "Log response size exceeded"),
        ];
        for mode in modes {
            let node = FakeNode::new(Vec::new(), u64::MAX).with_mode(mode);
            get_logs_chunked(&node.provider(), &filter(), 0, 3)
                .await
                .unwrap_err();
            assert_eq!(node.requests().last(), Some(&(0, 0)), "{mode:?}");
        }
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
