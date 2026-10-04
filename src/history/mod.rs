//! Historical chain reads over block ranges of any length.
//!
//! [`get_logs_chunked`] reads every log that matches a filter across a
//! block range, whatever limits the provider puts on `eth_getLogs`. On
//! top of it: [`beacon_prints`] and [`latest_beacon_prints`] build a
//! beacon's index series, `(block, timestamp, index)`;
//! [`market_events`] and [`latest_market_events`] replay a perp's whole
//! event history — the tape — through the same decoder the live feed
//! uses ([`crate::events::decode_log`]), position-NFT transfers
//! included — so [`OwnershipLog`] folds a tape into who held which
//! position when; [`market_tape`] reads the perp, its beacon and its pool's
//! liquidity changes in one chain order, which is everything a fold needs
//! to rebuild the market, and [`Replay`] is that fold, every fold here an
//! instance of [`Fold`]; and [`token_transfers`] reads an ERC-20's `Transfer` events
//! between address sets (for example, every USDC transfer between a
//! treasury and its wallets). [`History`] wraps them all with a uniform
//! block-lag policy and a request width learned once across scans.
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
//! A narrowing rejection is the scan working, not the endpoint failing —
//! providers deliver it either as a JSON-RPC error or with a client-error
//! status, and [`HftTransport`](crate::HftTransport) keeps such an answer
//! off the endpoint's health record and does not retry it, so a search
//! can narrow as many times as it needs to.
//!
//! The deployed beacons expose no last-update getter (`index()` returns
//! the value alone), so a beacon's newest `IndexUpdated` log is the only
//! record of when it last printed: `latest_beacon_prints(.., 1)` reads it.
//!
//! [`ContractError::LogsRejected`]: crate::errors::ContractError::LogsRejected

#![doc = "\n\nThe design of this module: [`src/history/DESIGN.md`](https://github.com/StrobeLabs/perpcity-rust-sdk/blob/main/src/history/DESIGN.md)."]

mod beacon;
mod fold;
mod replay;
pub(crate) mod scan;
mod tape;
mod transfers;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_support;

#[cfg(test)]
mod tests;

pub use beacon::{IndexPrint, beacon_prints, latest_beacon_prints};
pub use fold::{First, Fold, Latest, Stated};
pub use replay::{Gaps, PositionKind, PositionState, Positions, Replay};
pub use scan::{ScanStats, get_logs_chunked};
pub use tape::{
    ChainPoint, OwnershipLog, TapeAddresses, TapeEvent, latest_market_events, market_events,
    market_tape,
};
pub use transfers::{TokenTransfer, token_transfers};

use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};

use crate::constants::SNAPSHOT_BLOCK_LAG;
use crate::errors::Result;

/// Window requests a [`History`] handle keeps in flight by default: a
/// meaningful pipeline against typical provider latency while staying
/// polite to metered endpoints. Raise it with [`History::with_in_flight`]
/// against a provider you own.
pub const DEFAULT_IN_FLIGHT: usize = 4;

/// A handle over historical reads that owns what one-shot calls cannot:
/// the block-lag policy, the learned request width, the concurrency
/// budget, and an account of the work done.
///
/// **Lag.** Every reader takes `to_block: Option<u64>`; `None` reads to
/// the head minus the handle's lag ([`SNAPSHOT_BLOCK_LAG`] blocks unless
/// [`Self::with_lag`] says otherwise), so a lagging replica is never asked
/// for a block whose logs it may not have yet. Pass `Some(block)` to pin
/// a range instead — an already-final block needs no lag.
///
/// **Width.** The free functions re-learn the provider's `eth_getLogs`
/// limits on every call; the handle keeps the learned width across calls,
/// so a process that scans repeatedly (a collector) pays the search once.
///
/// **Concurrency.** The handle keeps up to [`DEFAULT_IN_FLIGHT`] window
/// requests outstanding per scan ([`Self::with_in_flight`] to change it);
/// the free functions stay sequential. Results are always delivered in
/// range order. Newest-first reads (`latest_*`) are sequential on every
/// path — they exist to stop early, and a request sent below the stopping
/// point is waste. Rate-limit backoff belongs in the transport, as ever.
///
/// **Telemetry.** [`Self::stats`] reports cumulative [`ScanStats`] over
/// every request the handle's scans have sent, so a long-lived process
/// can meter its reads and watch the provider; the free functions report
/// nothing.
///
/// Constructed from any [`Provider`] — reading history needs no signer.
/// A [`ChainReader`](crate::ChainReader) keeps one handle over its
/// provider, [`history()`](crate::ChainReader::history).
#[derive(Debug)]
pub struct History<P> {
    provider: P,
    lag: u64,
    in_flight: usize,
    widths: scan::SharedWidths,
}

impl<P: Provider> History<P> {
    /// A handle over `provider` with the default lag of
    /// [`SNAPSHOT_BLOCK_LAG`] blocks and [`DEFAULT_IN_FLIGHT`] concurrent
    /// window requests.
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            lag: SNAPSHOT_BLOCK_LAG,
            in_flight: DEFAULT_IN_FLIGHT,
            widths: scan::SharedWidths::new(),
        }
    }

    /// The handle with a different lag; `0` reads to the raw head.
    pub fn with_lag(mut self, blocks: u64) -> Self {
        self.lag = blocks;
        self
    }

    /// The handle with a different concurrency budget; `1` scans
    /// sequentially, exactly as the free functions do.
    pub fn with_in_flight(mut self, requests: usize) -> Self {
        self.in_flight = requests.max(1);
        self
    }

    /// Counters over every request this handle's scans have sent,
    /// cumulative since construction. Sample before and after a stretch
    /// of work and diff to meter it.
    pub fn stats(&self) -> ScanStats {
        self.widths.stats()
    }

    /// The newest block the handle reads by default: head minus the lag.
    ///
    /// # Errors
    ///
    /// The transport error from the head read.
    pub async fn tip(&self) -> Result<u64> {
        let head = self.provider.get_block_number().await?;
        Ok(head.saturating_sub(self.lag))
    }

    async fn resolve(&self, to_block: Option<u64>) -> Result<u64> {
        match to_block {
            Some(block) => Ok(block),
            None => self.tip().await,
        }
    }

    /// [`get_logs_chunked`], to `to_block` or the lagged head.
    ///
    /// # Errors
    ///
    /// As [`get_logs_chunked`].
    pub async fn logs(
        &self,
        filter: &Filter,
        from_block: u64,
        to_block: Option<u64>,
    ) -> Result<Vec<Log>> {
        let to = self.resolve(to_block).await?;
        scan::scan_all(
            &self.provider,
            filter,
            from_block,
            to,
            &self.widths,
            self.in_flight,
        )
        .await
    }

    /// [`beacon_prints`], to `to_block` or the lagged head.
    ///
    /// # Errors
    ///
    /// As [`beacon_prints`].
    pub async fn beacon_prints(
        &self,
        beacon: Address,
        from_block: u64,
        to_block: Option<u64>,
    ) -> Result<Vec<IndexPrint>> {
        let to = self.resolve(to_block).await?;
        beacon::beacon_prints_with(
            &self.provider,
            beacon,
            from_block,
            to,
            &self.widths,
            self.in_flight,
        )
        .await
    }

    /// [`latest_beacon_prints`], to `to_block` or the lagged head.
    ///
    /// # Errors
    ///
    /// As [`latest_beacon_prints`].
    pub async fn latest_beacon_prints(
        &self,
        beacon: Address,
        from_block: u64,
        to_block: Option<u64>,
        limit: usize,
    ) -> Result<Vec<IndexPrint>> {
        let to = self.resolve(to_block).await?;
        beacon::latest_beacon_prints_with(
            &self.provider,
            beacon,
            from_block,
            to,
            limit,
            &self.widths,
        )
        .await
    }

    /// [`market_events`], to `to_block` or the lagged head.
    ///
    /// # Errors
    ///
    /// As [`market_events`].
    pub async fn market_events(
        &self,
        perp: Address,
        from_block: u64,
        to_block: Option<u64>,
    ) -> Result<Vec<TapeEvent>> {
        let to = self.resolve(to_block).await?;
        tape::market_events_with(
            &self.provider,
            perp,
            from_block,
            to,
            &self.widths,
            self.in_flight,
        )
        .await
    }

    /// [`market_tape`], to `to_block` or the lagged head.
    ///
    /// # Errors
    ///
    /// As [`market_tape`].
    pub async fn market_tape(
        &self,
        addresses: TapeAddresses,
        from_block: u64,
        to_block: Option<u64>,
    ) -> Result<Vec<TapeEvent>> {
        let to = self.resolve(to_block).await?;
        tape::market_tape_with(
            &self.provider,
            addresses,
            from_block,
            to,
            &self.widths,
            self.in_flight,
        )
        .await
    }

    /// [`latest_market_events`], to `to_block` or the lagged head.
    ///
    /// # Errors
    ///
    /// As [`latest_market_events`].
    pub async fn latest_market_events(
        &self,
        perp: Address,
        from_block: u64,
        to_block: Option<u64>,
        limit: usize,
    ) -> Result<Vec<TapeEvent>> {
        let to = self.resolve(to_block).await?;
        tape::latest_market_events_with(&self.provider, perp, from_block, to, limit, &self.widths)
            .await
    }

    /// [`token_transfers`], to `to_block` or the lagged head.
    ///
    /// # Errors
    ///
    /// As [`token_transfers`].
    pub async fn token_transfers(
        &self,
        token: Address,
        senders: Option<&[Address]>,
        recipients: Option<&[Address]>,
        from_block: u64,
        to_block: Option<u64>,
    ) -> Result<Vec<TokenTransfer>> {
        let to = self.resolve(to_block).await?;
        transfers::token_transfers_with(
            &self.provider,
            token,
            senders,
            recipients,
            from_block,
            to,
            &self.widths,
            self.in_flight,
        )
        .await
    }
}
