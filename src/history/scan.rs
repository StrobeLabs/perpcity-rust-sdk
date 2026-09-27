//! The adaptive `eth_getLogs` scan: chunking, width learning, and failure
//! classification. See the [module docs](super) for the strategy.

use alloy::providers::Provider;
use alloy::rpc::json_rpc::ErrorPayload;
use alloy::rpc::types::{Filter, Log};
use alloy::transports::{RpcError, TransportError, TransportErrorKind};

use crate::constants::{LOG_SCAN_INITIAL_SPAN, LOG_SCAN_MAX_SPAN};
use crate::errors::{ContractError, Result, ValidationError};

/// Every log matching `filter` in blocks `from_block..=to_block`, in chain
/// order.
///
/// The block range of `filter` itself is ignored. See the
/// [module docs](super) for how the range is split into requests.
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

/// A block range read in adaptive chunks, from either end.
pub(super) struct LogScan<'a, P> {
    provider: &'a P,
    filter: Filter,
    /// Blocks not yet read, inclusive; `None` once the range is done.
    remaining: Option<(u64, u64)>,
    widths: WidthSearch,
}

impl<'a, P: Provider> LogScan<'a, P> {
    pub(super) fn new(
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
    pub(super) async fn next_chunk(&mut self, end: End) -> Result<Option<Vec<Log>>> {
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

/// Which end of the unread range the next chunk comes from.
#[derive(Clone, Copy)]
pub(super) enum End {
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
