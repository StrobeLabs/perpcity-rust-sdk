//! Live market discovery over WebSocket.
//!
//! [`PerpCreatedFeed`] subscribes to the `PerpCreated` logs of a factory
//! list (both factory shapes) and yields each new market as a
//! [`PerpCreation`]. It covers creations from the moment it subscribes;
//! [`list_perps`](crate::discovery::list_perps) covers the history.
//!
//! To follow a factory list without a gap, subscribe first, then call
//! `list_perps`, and merge the two by [`PerpCreation::perp`]. After a
//! disconnect (`next()` returns `None`), subscribe again and list from the
//! last block seen.
//!
//! # Example
//!
//! ```rust,no_run
//! use perpcity_sdk::discovery::list_perps;
//! use perpcity_sdk::feeds::PerpCreatedFeed;
//! use perpcity_sdk::transport::ws::{ReconnectConfig, WsManager};
//! use alloy::primitives::address;
//!
//! # async fn example() -> perpcity_sdk::Result<()> {
//! let factories = [address!("CE0c5f65A5eDa69A1dFb3f3273749B649abc4eC6")];
//! let ws = WsManager::connect("wss://arb-rpc.example.com", ReconnectConfig::default()).await?;
//!
//! let mut feed = PerpCreatedFeed::subscribe(&ws, &factories).await?;
//! let known = list_perps(ws.provider(), &factories, 468_050_231).await?;
//! while let Some(created) = feed.next().await {
//!     if !known.iter().any(|p| p.perp == created.perp) {
//!         println!("new market {} at block {}", created.perp, created.block_number);
//!     }
//! }
//! # Ok(())
//! # }
//! ```

use alloy::primitives::Address;
use alloy::rpc::types::Log;
use tokio::sync::mpsc;

use crate::discovery::{PerpCreation, decode_perp_created, perp_created_filter};
use crate::transport::ws::WsManager;

/// A stream of new perp markets from a factory list.
///
/// Created via [`PerpCreatedFeed::subscribe()`]. Returns `None` from
/// [`next()`](PerpCreatedFeed::next) when the WebSocket connection is
/// lost.
#[derive(Debug)]
pub struct PerpCreatedFeed {
    rx: mpsc::Receiver<Log>,
}

impl PerpCreatedFeed {
    /// Subscribe to the `PerpCreated` logs of `factories`.
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidConfig`](crate::ValidationError::InvalidConfig)
    /// for an empty factory list or a zero address in it, or the
    /// subscription error.
    pub async fn subscribe(ws: &WsManager, factories: &[Address]) -> crate::Result<Self> {
        let filter = perp_created_filter(factories)?;
        let rx = ws.subscribe_logs(filter).await?;
        tracing::debug!(?factories, "perp creation feed subscribed");
        Ok(Self { rx })
    }

    /// Receive the next new market.
    ///
    /// Waits until a creation arrives. A log that a reorg removed, or one
    /// that fails to decode, is skipped.
    pub async fn next(&mut self) -> Option<PerpCreation> {
        loop {
            let log = self.rx.recv().await?;
            if let Some(created) = live_creation(&log) {
                return Some(created);
            }
        }
    }
}

fn live_creation(log: &Log) -> Option<PerpCreation> {
    if log.removed {
        return None;
    }
    decode_perp_created(log)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAINNET_LOG: &str = include_str!("../../tests/fixtures/perp_created_arbitrum_one.json");

    #[test]
    fn a_reorged_creation_is_skipped() {
        let mut log: Log = serde_json::from_str(MAINNET_LOG).unwrap();
        assert!(live_creation(&log).is_some());
        log.removed = true;
        assert!(live_creation(&log).is_none());
    }
}
