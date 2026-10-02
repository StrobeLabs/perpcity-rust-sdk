//! Live market event feed over WebSocket.
//!
//! [`MarketFeed`] subscribes to a single `Perp` market and its `Beacon` via
//! [`WsManager`], and decodes raw logs into typed [`MarketEvent`] values.
//! Consumers call [`MarketFeed::next()`] in a loop to receive real-time market
//! data with zero per-read RPC cost.
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
use alloy::rpc::types::{Filter, Log};
use tokio::sync::mpsc;

use crate::errors::ValidationError;
use crate::events::{MarketEvent, decode_log};
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
        loop {
            let log = self.rx.recv().await?;
            match decode_log(&log) {
                Ok(Some(event)) => {
                    tracing::trace!(perp = %self.perp, event = ?event, "market event received");
                    return Some(Ok(event));
                }
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(
                        perp = %self.perp, error = %e,
                        "market event recognised but undecodable"
                    );
                    return Some(Err(e));
                }
            }
        }
    }

    /// The `Perp` market address this feed is subscribed to.
    pub fn perp(&self) -> Address {
        self.perp
    }
}
