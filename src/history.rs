//! Historical chain reads over block ranges of any length.
//!
//! [`get_logs_chunked`] reads every log that matches a filter across a
//! block range, whatever limits the provider puts on `eth_getLogs`.
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

use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use alloy::transports::{RpcError, TransportError, TransportErrorKind};

use crate::constants::{LOG_SCAN_INITIAL_SPAN, LOG_SCAN_MAX_SPAN};
use crate::errors::{Result, ValidationError};

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
    while let Some(chunk) = scan.next_oldest().await? {
        logs.extend(chunk);
    }
    Ok(logs)
}

/// A block range read in adaptive chunks.
pub(crate) struct LogScan<'a, P> {
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
    pub(crate) fn new(
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

    /// The logs of the oldest unread chunk, or `None` once the range is
    /// read.
    pub(crate) async fn next_oldest(&mut self) -> Result<Option<Vec<Log>>> {
        let Some((low, high)) = self.remaining else {
            return Ok(None);
        };
        loop {
            let reach = self.span - 1;
            let (from, to) = (low, low.saturating_add(reach).min(high));
            let filter = self.filter.clone().from_block(from).to_block(to);
            match self.provider.get_logs(&filter).await {
                Ok(logs) => {
                    self.remaining = (to < high).then(|| (to + 1, high));
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
    use alloy::primitives::{Address, B256, Bytes};

    use super::*;
    use crate::PerpCityError;
    use crate::test_support::{FakeNode, Mode, mined_log};

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
