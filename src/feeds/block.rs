//! Live block header feed over WebSocket.
//!
//! [`BlockHeaderFeed`] subscribes to new block headers via [`WsManager`],
//! yielding each header as it arrives. Consumers typically extract
//! `base_fee_per_gas` and feed it to [`PerpClient::set_base_fee()`] for
//! zero-RPC gas price updates.
//!
//! # Example
//!
//! ```rust,no_run
//! use perpcity_sdk::feeds::BlockHeaderFeed;
//! use perpcity_sdk::transport::ws::{WsManager, ReconnectConfig};
//!
//! # async fn example() -> perpcity_sdk::Result<()> {
//! let ws = WsManager::connect("wss://base-rpc.example.com", ReconnectConfig::default()).await?;
//!
//! let mut feed = BlockHeaderFeed::subscribe(&ws).await?;
//! while let Some(header) = feed.next().await {
//!     if let Some(base_fee) = header.base_fee_per_gas {
//!         println!("base fee: {base_fee}");
//!     }
//! }
//! # Ok(())
//! # }
//! ```

use alloy::rpc::types::Header;
use tokio::sync::mpsc;

use crate::transport::ws::WsManager;

/// A stream of block headers from a WebSocket `newHeads` subscription.
///
/// Created via [`BlockHeaderFeed::subscribe()`]. Call
/// [`next()`](BlockHeaderFeed::next) in a loop to receive headers.
/// Returns `None` when the WebSocket connection is lost.
#[derive(Debug)]
pub struct BlockHeaderFeed {
    rx: mpsc::Receiver<Header>,
}

impl BlockHeaderFeed {
    /// Subscribe to new block headers.
    pub async fn subscribe(ws: &WsManager) -> crate::Result<Self> {
        let rx = ws.subscribe_blocks().await?;
        tracing::info!("block header feed subscribed");
        Ok(Self { rx })
    }

    /// Receive the next block header.
    ///
    /// Blocks until a new header arrives. Returns `None` when the
    /// WebSocket connection is lost (sender dropped).
    pub async fn next(&mut self) -> Option<Header> {
        self.rx.recv().await
    }
}
