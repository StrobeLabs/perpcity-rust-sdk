//! Live market event feed over WebSocket.
//!
//! [`MarketFeed`] subscribes to a single `Perp` market and its `Beacon` via
//! [`WsManager`], and decodes raw logs into typed [`MarketEvent`] values.
//! Consumers call [`MarketFeed::next()`] in a loop to receive real-time market
//! data with zero per-read RPC cost, or [`MarketFeed::next_stamped()`] for
//! the same event as the tape row a scan would have produced.
//!
//! There is no `perp_id`: each market is its own `Perp` contract, so the
//! address filter alone scopes the stream to one market (plus its beacon's
//! `IndexUpdated`).
//!
//! # Example
//!
//! ```rust,no_run
//! use perpcity_sdk::feeds::MarketFeed;
//! use perpcity_sdk::transport::ws::{WsManager, ReconnectConfig};
//! use alloy::primitives::{Address, address};
//!
//! # async fn example() -> perpcity_sdk::Result<()> {
//! let ws = WsManager::connect("wss://arb-rpc.example.com", ReconnectConfig::default()).await?;
//!
//! let perp = address!("0000000000000000000000000000000000000001");
//! let beacon = address!("0000000000000000000000000000000000000002");
//!
//! let mut feed = MarketFeed::subscribe(&ws, perp, beacon).await?;
//! while let Some(event) = feed.next().await {
//!     println!("{event:?}");
//! }
//! # Ok(())
//! # }
//! ```

use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use tokio::sync::mpsc;

use crate::errors::ValidationError;
use crate::events::{MarketEvent, decode_log};
use crate::history::TapeEvent;
use crate::history::scan::block_timestamps;
use crate::transport::ws::WsManager;

/// A filtered stream of decoded [`MarketEvent`]s for a single perp.
///
/// Created via [`MarketFeed::subscribe()`]. Call [`next()`](MarketFeed::next)
/// in a loop to receive events. Returns `None` when the WebSocket
/// connection is lost.
#[derive(Debug)]
pub struct MarketFeed {
    rx: mpsc::Receiver<Log>,
    perp: Address,
}

impl MarketFeed {
    /// Subscribe to events for a single perp market.
    ///
    /// Creates a WebSocket log subscription filtered to the `perp` (market) and
    /// `beacon` contract addresses. The `Perp` address alone scopes the stream
    /// to this market; the beacon address adds its `IndexUpdated` events.
    pub async fn subscribe(ws: &WsManager, perp: Address, beacon: Address) -> crate::Result<Self> {
        let filter = Filter::new().address(vec![perp, beacon]);
        let rx = ws.subscribe_logs(filter).await?;
        tracing::debug!(%perp, %beacon, "market feed subscribed");
        Ok(Self { rx, perp })
    }

    /// Receive the next decoded event for this market.
    ///
    /// Blocks until a recognized event arrives. `None` means the WebSocket
    /// connection is lost and no further event will come. Unrecognized logs
    /// (admin, governance, pool-internal) are skipped silently, because
    /// there is nothing wrong with them.
    ///
    /// A `Some(Err(_))` is a log of *this* vocabulary that would not decode.
    /// It is surfaced rather than skipped: the feed would otherwise show a
    /// caller a stream with a hole in it, and only the caller can decide
    /// whether to carry on. The feed does carry on — the next `next` reads
    /// the following log.
    pub async fn next(&mut self) -> Option<Result<MarketEvent, ValidationError>> {
        let (_, decoded) = self.next_decoded().await?;
        Some(decoded)
    }

    /// [`Self::next`], with the event stamped as the tape would stamp it:
    /// block, block hash, log index, timestamp and transaction.
    ///
    /// This is the feed for a fold that orders on chain point or pairs the
    /// logs of one transaction: each row is the [`TapeEvent`] a scan would
    /// have built from the same log, so the fold never knows which tense fed
    /// it. The event *set* is the subscription's, the perp and its beacon;
    /// the PoolManager's liquidity changes that
    /// [`History::market_tape`](crate::history::History::market_tape) also
    /// carries are not on this feed, so a fold that needs the book live
    /// follows the lagged tail through the handle until a feed over all
    /// three addresses exists. When the subscription's log omits its block
    /// timestamp, the header is read from `provider`, once, as a scan reads
    /// it.
    ///
    /// `None` and `Some(Err)` as on [`Self::next`]; the header read's own
    /// failure comes back as its error.
    pub async fn next_stamped<P: Provider>(
        &mut self,
        provider: &P,
    ) -> Option<crate::Result<TapeEvent>> {
        let (log, decoded) = self.next_decoded().await?;
        Some(
            async {
                let event = decoded?;
                let timestamp = match log.block_timestamp {
                    Some(timestamp) => timestamp,
                    None => {
                        let number =
                            log.block_number
                                .ok_or_else(|| ValidationError::DecodeFailed {
                                    context: format!(
                                        "market event log from {} is not mined",
                                        log.address()
                                    ),
                                })?;
                        block_timestamps(provider, std::iter::once(&log)).await?[&number]
                    }
                };
                Ok(TapeEvent::stamped(&log, event, timestamp)?)
            }
            .await,
        )
    }

    /// The next log of this vocabulary and what it decoded to, skipping
    /// the logs that are not ours.
    async fn next_decoded(&mut self) -> Option<(Log, Result<MarketEvent, ValidationError>)> {
        loop {
            let log = self.rx.recv().await?;
            match decode_log(&log) {
                Ok(Some(event)) => {
                    tracing::trace!(perp = %self.perp, event = ?event, "market event received");
                    return Some((log, Ok(event)));
                }
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(
                        perp = %self.perp, error = %e,
                        "market event recognised but undecodable"
                    );
                    return Some((log, Err(e)));
                }
            }
        }
    }

    /// The `Perp` market address this feed is subscribed to.
    pub fn perp(&self) -> Address {
        self.perp
    }
}
