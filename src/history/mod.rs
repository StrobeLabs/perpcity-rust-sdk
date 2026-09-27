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
//!
//! [`ContractError::LogsRejected`]: crate::errors::ContractError::LogsRejected

mod beacon;
mod scan;
mod transfers;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_support;

#[cfg(test)]
mod tests;

pub use beacon::{IndexPrint, beacon_prints, latest_beacon_prints};
pub use scan::get_logs_chunked;
pub use transfers::{TokenTransfer, token_transfers};
