//! The adaptive `eth_getLogs` scan: chunking, width learning, failure
//! classification, and the concurrent drivers. See the
//! [module docs](super) for the strategy.
//!
//! A scan carves the range into **windows** at the width the shared
//! [`WidthSearch`] currently believes and keeps a bounded number of them
//! in flight; results are delivered in range order. Each window is read
//! completely by its own worker — a window that turns out too wide
//! narrows *itself* into sub-requests (reporting to the shared width so
//! later carves start narrower) without disturbing its siblings. Windows
//! carved while earlier ones are still in flight use the estimate as it
//! stands; the learning applies to every carve after an answer lands.
//!
//! A newest-first scan ([`scan_newest`]) is not windowed: it exists to
//! stop early, so it sends one request at a time from the unread top and
//! lets the caller stop the moment it holds enough.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use alloy::transports::{RpcError, TransportError, TransportErrorKind};
use futures_util::stream::{self, Stream, StreamExt, TryStreamExt};

use crate::constants::{LOG_SCAN_INITIAL_SPAN, LOG_SCAN_MAX_SPAN};
use crate::errors::{ContractError, PerpCityError, Result, ValidationError};
use crate::transport::fault::is_rate_limit;

/// Concurrent header reads when a provider omits log timestamps.
const HEADER_READ_CONCURRENCY: usize = 4;

/// The timestamp of every block among `logs` that arrived without one,
/// read from its header once, with bounded concurrency.
///
/// # Errors
///
/// [`ContractError::BlockUnavailable`] for a header the provider does not
/// hold, or the transport error from a header read.
pub(crate) async fn block_timestamps<'a, P: Provider>(
    provider: &P,
    logs: impl Iterator<Item = &'a Log>,
) -> Result<HashMap<u64, u64>> {
    let missing: BTreeSet<u64> = logs
        .filter(|log| log.block_timestamp.is_none())
        .filter_map(|log| log.block_number)
        .collect();
    stream::iter(missing)
        .map(|number| async move {
            let block = provider
                .get_block_by_number(number.into())
                .await?
                .ok_or(ContractError::BlockUnavailable { number })?;
            Ok::<_, PerpCityError>((number, block.header.timestamp))
        })
        .buffer_unordered(HEADER_READ_CONCURRENCY)
        .try_collect()
        .await
}

/// Every log matching `filter` in blocks `from_block..=to_block`, in chain
/// order.
///
/// The block range of `filter` itself is ignored. Sequential — one
/// request in flight; a [`History`](super::History) handle scans the same
/// way with its concurrency budget. See the [module docs](super) for how
/// the range is split into requests.
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
    scan_all(
        provider,
        filter,
        from_block,
        to_block,
        &SharedWidths::new(),
        1,
    )
    .await
}

/// Every log in the range, in chain order, with up to `in_flight` window
/// requests outstanding at once.
pub(super) async fn scan_all<P: Provider>(
    provider: &P,
    filter: &Filter,
    from_block: u64,
    to_block: u64,
    widths: &SharedWidths,
    in_flight: usize,
) -> Result<Vec<Log>> {
    check_block_range(from_block, to_block)?;
    let mut chunks = std::pin::pin!(
        windows_oldest_first(from_block, to_block, widths)
            .map(|(low, high)| read_window(provider, filter, low, high, widths))
            .buffered(in_flight.max(1))
    );
    let mut logs = Vec::new();
    while let Some(chunk) = chunks.try_next().await? {
        logs.extend(chunk);
    }
    Ok(logs)
}

/// The range's chunks newest first (each chunk's logs in chain order),
/// one request at a time, anchored at the unread top.
///
/// The caller validates the range and stops pulling once it has enough.
/// Sequential by design: a request sent below the stopping point is pure
/// waste, and a rejection narrows the request from its top instead of
/// committing to a whole window.
pub(super) fn scan_newest<'a, P: Provider>(
    provider: &'a P,
    filter: &'a Filter,
    from_block: u64,
    to_block: u64,
    widths: &'a SharedWidths,
) -> impl Stream<Item = Result<Vec<Log>>> + 'a {
    stream::try_unfold(Some(to_block), move |high| async move {
        let Some(high) = high.filter(|&high| high >= from_block) else {
            return Ok(None);
        };
        loop {
            let low = high.saturating_sub(widths.next() - 1).max(from_block);
            if let Some(chunk) = attempt(provider, filter, low, high, widths).await? {
                return Ok(Some((chunk, low.checked_sub(1))));
            }
        }
    })
}

/// Windows tiling `from..=to` oldest first, each carved at the width the
/// shared search believes when the driver asks for it.
fn windows_oldest_first<'a>(
    from: u64,
    to: u64,
    widths: &'a SharedWidths,
) -> impl Stream<Item = (u64, u64)> + 'a {
    stream::unfold(Some(from), move |low| {
        std::future::ready(low.filter(|&low| low <= to).map(|low| {
            let high = low.saturating_add(widths.next() - 1).min(to);
            ((low, high), high.checked_add(1))
        }))
    })
}

/// Read the window `from..=to` completely, in chain order.
///
/// A range rejection narrows this window's own requests (and the shared
/// width, so later carves start narrower) without disturbing sibling
/// windows.
async fn read_window<P: Provider>(
    provider: &P,
    filter: &Filter,
    from: u64,
    to: u64,
    widths: &SharedWidths,
) -> Result<Vec<Log>> {
    let mut logs = Vec::new();
    let mut low = from;
    loop {
        let high = low + (widths.next() - 1).min(to - low);
        if let Some(chunk) = attempt(provider, filter, low, high, widths).await? {
            logs.extend(chunk);
            if high == to {
                return Ok(logs);
            }
            low = high + 1;
        }
    }
}

/// One `eth_getLogs` request over `from..=to`, reported to the shared
/// width. `None` is a range rejection a narrower request may fix (the
/// width is already narrowed); failures no narrower range fixes are
/// returned at once.
async fn attempt<P: Provider>(
    provider: &P,
    filter: &Filter,
    from: u64,
    to: u64,
    widths: &SharedWidths,
) -> Result<Option<Vec<Log>>> {
    let request = filter.clone().from_block(from).to_block(to);
    let started = Instant::now();
    let answer = provider.get_logs(&request).await;
    widths.tally(&answer, started.elapsed());
    match answer {
        Ok(chunk) => {
            widths.accepted(to - from + 1);
            Ok(Some(chunk))
        }
        Err(error) => match classify(&error) {
            Failure::Range if to > from => {
                tracing::debug!(from, to, %error, "eth_getLogs range rejected; narrowing");
                widths.rejected(to - from + 1);
                Ok(None)
            }
            Failure::Range | Failure::Refused => Err(ContractError::LogsRejected {
                from_block: from,
                to_block: to,
                source: error,
            }
            .into()),
            Failure::Unanswered => Err(error.into()),
        },
    }
}

pub(super) fn check_block_range(
    from_block: u64,
    to_block: u64,
) -> std::result::Result<(), ValidationError> {
    if from_block > to_block {
        return Err(ValidationError::InvalidBlockRange {
            from_block,
            to_block,
        });
    }
    Ok(())
}

/// Counters over the requests a scan driver sent. Cumulative for the
/// life of the [`History`](super::History) handle that exposes them
/// ([`History::stats`](super::History::stats)); sample and diff to meter
/// a stretch of work. The free functions report nothing — their width
/// search, and these counters with it, live only for the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScanStats {
    /// `eth_getLogs` requests sent.
    pub requests: u64,
    /// Requests answered with an error instead of logs: range rejections
    /// that narrowed the scan, and the failures that ended it.
    pub rejections: u64,
    /// Logs returned across all accepted requests.
    pub logs: u64,
    /// Logs whose topic this vocabulary covers and which would not decode:
    /// a binding that disagrees with the shape on chain, or a value too wide
    /// to hold. Counted rather than dropped, because a tape that quietly
    /// shrinks is worse than one that says what it is missing. A non-zero
    /// count here means the tape has a gap; a scan is not failed over it,
    /// since one such log should not cost a scan of millions of blocks.
    pub undecodable: u64,
    /// Width the next request would use, in blocks — the search's
    /// current belief about the provider's `eth_getLogs` limit.
    pub learned_width: u64,
    /// Total time spent awaiting `eth_getLogs` answers. Summed across
    /// concurrent requests, so it can exceed wall time.
    pub request_time: Duration,
}

/// A [`WidthSearch`] and its [`ScanStats`], shared by every worker of a
/// scan (and, on a [`History`](super::History) handle, across scans).
/// Lock scope is one bookkeeping call; it is never held across an await.
#[derive(Debug)]
pub(super) struct SharedWidths(Mutex<State>);

#[derive(Debug)]
struct State {
    search: WidthSearch,
    stats: ScanStats,
}

impl SharedWidths {
    pub(super) fn new() -> Self {
        Self(Mutex::new(State {
            search: WidthSearch::new(),
            stats: ScanStats::default(),
        }))
    }

    /// Width for the next carve or request; at least 1.
    fn next(&self) -> u64 {
        self.0.lock().unwrap().search.next
    }

    fn accepted(&self, width: u64) {
        self.0.lock().unwrap().search.accepted(width);
    }

    fn rejected(&self, width: u64) {
        self.0.lock().unwrap().search.rejected(width);
    }

    /// Counts logs this vocabulary recognised and could not decode.
    pub(super) fn undecodable(&self, how_many: u64) {
        self.0.lock().unwrap().stats.undecodable += how_many;
    }

    /// Counts one request and its answer.
    fn tally(&self, answer: &std::result::Result<Vec<Log>, TransportError>, elapsed: Duration) {
        let mut state = self.0.lock().unwrap();
        state.stats.requests += 1;
        state.stats.request_time += elapsed;
        match answer {
            Ok(chunk) => state.stats.logs += chunk.len() as u64,
            Err(_) => state.stats.rejections += 1,
        }
    }

    /// The counters so far, with the width the search currently believes.
    pub(super) fn stats(&self) -> ScanStats {
        let state = self.0.lock().unwrap();
        ScanStats {
            learned_width: state.search.next,
            ..state.stats
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
/// span, so the contradicted bound is dropped. Under a concurrent scan
/// the answers arrive interleaved; the same contradiction handling keeps
/// the estimate self-correcting.
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
