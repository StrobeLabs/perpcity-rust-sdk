//! Live market event feed over WebSocket.
//!
//! [`MarketFeed`] subscribes to PerpManager and Beacon contract events
//! via [`WsManager`], decodes raw logs into typed [`MarketEvent`] values,
//! and filters by perp. Consumers call [`MarketFeed::next()`] in a loop
//! to receive real-time market data with zero per-read RPC cost.
//!
//! # Example
//!
//! ```rust,no_run
//! use perpcity_sdk::feed::MarketFeed;
//! use perpcity_sdk::transport::ws::{WsManager, ReconnectConfig};
//! use alloy::primitives::{Address, B256, address};
//!
//! # async fn example() -> perpcity_sdk::Result<()> {
//! let ws = WsManager::connect("wss://base-rpc.example.com", ReconnectConfig::default()).await?;
//!
//! let perp_manager = address!("0000000000000000000000000000000000000001");
//! let beacon = address!("0000000000000000000000000000000000000002");
//! let perp_id = B256::ZERO;
//!
//! let mut feed = MarketFeed::subscribe(&ws, perp_manager, beacon, perp_id).await?;
//! while let Some(event) = feed.next().await {
//!     println!("{event:?}");
//! }
//! # Ok(())
//! # }
//! ```

use alloy::primitives::{Address, B256};
use alloy::rpc::types::{Filter, Log};
use tokio::sync::mpsc;

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
    perp_id: B256,
}

impl MarketFeed {
    /// Subscribe to events for a single perp.
    ///
    /// Creates a WebSocket log subscription filtered to the `perp_manager`
    /// and `beacon` contract addresses. PerpManager events are further
    /// filtered by `perp_id` in [`next()`](MarketFeed::next). Beacon
    /// `IndexUpdated` events are already scoped to this perp by the
    /// beacon address filter.
    pub async fn subscribe(
        ws: &WsManager,
        perp_manager: Address,
        beacon: Address,
        perp_id: B256,
    ) -> crate::Result<Self> {
        let filter = Filter::new().address(vec![perp_manager, beacon]);
        let rx = ws.subscribe_logs(filter).await?;
        Ok(Self { rx, perp_id })
    }

    /// Receive the next decoded event for this perp.
    ///
    /// Blocks until a matching event arrives. Returns `None` when the
    /// WebSocket connection is lost (sender dropped).
    ///
    /// Skips unrecognized events and events belonging to other perps.
    pub async fn next(&mut self) -> Option<MarketEvent> {
        loop {
            let log = self.rx.recv().await?;
            if let Some(event) = decode_log(&log) {
                match &event {
                    MarketEvent::PositionOpened { perp_id, .. }
                    | MarketEvent::NotionalAdjusted { perp_id, .. }
                    | MarketEvent::PositionClosed { perp_id, .. }
                        if *perp_id != self.perp_id =>
                    {
                        continue;
                    }
                    _ => return Some(event),
                }
            }
        }
    }

    /// The perp ID this feed is filtering for.
    pub fn perp_id(&self) -> B256 {
        self.perp_id
    }
}
