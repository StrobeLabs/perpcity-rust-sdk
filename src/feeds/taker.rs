//! Block-atomic live taker liquidity snapshots.
//!
//! The refresh path may use a handful of batched RPC reads, but consumers quote
//! entirely in memory. A new snapshot is published only after every read has
//! succeeded against one canonical block hash.

use std::sync::Arc;

use alloy::primitives::{Address, B256};
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
    /// This is useful for tests, historical simulation, and legacy deployments
    /// whose module bounds are reconstructed by an external checkpoint worker.
    pub fn from_snapshot(snapshot: TakerMarketSnapshot) -> (Self, LiveTakerMarketPublisher) {
        let (tx, latest) = watch::channel(Arc::new(snapshot));
        (Self { latest }, LiveTakerMarketPublisher { tx })
    }

    /// Load immediately, then refresh atomically on every WebSocket `newHeads`
    /// notification. Failed refreshes leave the last good snapshot published.
    pub async fn subscribe(
        client: Arc<PerpClient>,
        ws: &WsManager,
        pool_manager: Address,
    ) -> crate::Result<Self> {
        let initial = client.load_taker_market_snapshot(pool_manager).await?;
        let (market, publisher) = Self::from_snapshot(initial);
        let mut blocks = ws.subscribe_blocks().await?;
        tokio::spawn(async move {
            while blocks.recv().await.is_some() {
                match client.load_taker_market_snapshot(pool_manager).await {
                    Ok(snapshot) => publisher.publish(snapshot),
                    Err(error) => tracing::warn!(%error, "taker snapshot refresh failed"),
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
        self.latest.borrow().block_hash == block_hash
    }
}

/// Producer handle for externally maintained or legacy market snapshots.
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
        let mut second = TakerMarketSnapshot::default();
        second.block_number = 2;
        second.block_hash = B256::with_last_byte(2);
        publisher.publish(second);
        assert_eq!(market.latest().block_number, 2);
        assert!(market.is_current(B256::with_last_byte(2)));
    }
}
