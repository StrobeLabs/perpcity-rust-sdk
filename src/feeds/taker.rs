//! Block-atomic live taker liquidity snapshots.
//!
//! The refresh path may use a handful of batched RPC reads, but consumers quote
//! entirely in memory. A new snapshot is published only after every read has
//! succeeded against one canonical block hash.

use std::sync::Arc;

use alloy::primitives::B256;
use tokio::sync::watch;

use crate::PerpClient;
use crate::math::swap::TakerMarketSnapshot;
use crate::transport::ws::WsManager;

/// Shared, read-optimized view of a Perp market's taker liquidity.
#[derive(Debug, Clone)]
pub struct LiveTakerMarket {
    latest: watch::Receiver<Arc<TakerMarketSnapshot>>,
}

impl LiveTakerMarket {
    /// Create a detached cache from an already validated snapshot.
    ///
    /// This is useful for tests, historical simulation, and externally
    /// maintained snapshots.
    pub fn from_snapshot(snapshot: TakerMarketSnapshot) -> (Self, LiveTakerMarketPublisher) {
        let (tx, latest) = watch::channel(Arc::new(snapshot));
        (Self { latest }, LiveTakerMarketPublisher { tx })
    }

    /// Load immediately, then refresh atomically on every WebSocket `newHeads`
    /// notification. Failed refreshes leave the last good snapshot published.
    pub async fn subscribe(client: Arc<PerpClient>, ws: &WsManager) -> crate::Result<Self> {
        let initial = client.load_taker_market_snapshot().await?;
        let (market, publisher) = Self::from_snapshot(initial);
        let mut blocks = ws.subscribe_blocks().await?;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Wind down once every consumer handle is gone.
                    _ = publisher.tx.closed() => break,
                    head = blocks.recv() => {
                        if head.is_none() {
                            break;
                        }
                        // Coalesce queued heads: a reload can outlast several
                        // block times, and the loader pins the latest
                        // canonical block regardless, so a backlog is purely
                        // redundant RPC work.
                        while blocks.try_recv().is_ok() {}
                        match client.load_taker_market_snapshot().await {
                            Ok(snapshot) => publisher.publish(snapshot),
                            Err(error) => {
                                tracing::warn!(%error, "taker snapshot refresh failed");
                            }
                        }
                    }
                }
            }
        });
        Ok(market)
    }

    /// Latest complete snapshot. Cloning is an atomic reference-count bump.
    pub fn latest(&self) -> Arc<TakerMarketSnapshot> {
        self.latest.borrow().clone()
    }

    /// Subscribe to snapshot changes.
    pub fn changes(&self) -> watch::Receiver<Arc<TakerMarketSnapshot>> {
        self.latest.clone()
    }

    /// Whether `block_hash` is still the quoteable head in this cache.
    pub fn is_current(&self, block_hash: B256) -> bool {
        self.latest.borrow().block.hash == block_hash
    }
}

/// Producer handle for externally maintained market snapshots.
#[derive(Debug, Clone)]
pub struct LiveTakerMarketPublisher {
    tx: watch::Sender<Arc<TakerMarketSnapshot>>,
}

impl LiveTakerMarketPublisher {
    /// Atomically replace the visible snapshot.
    pub fn publish(&self, snapshot: TakerMarketSnapshot) {
        self.tx.send_replace(Arc::new(snapshot));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishes_complete_snapshots() {
        let first = TakerMarketSnapshot::default();
        let (market, publisher) = LiveTakerMarket::from_snapshot(first);
        let second = TakerMarketSnapshot {
            block: crate::math::BlockContext {
                number: 2,
                hash: B256::with_last_byte(2),
                timestamp: 0,
            },
            ..Default::default()
        };
        publisher.publish(second);
        assert_eq!(market.latest().block.number, 2);
        assert!(market.is_current(B256::with_last_byte(2)));
    }
}
