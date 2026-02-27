//! WebSocket subscription manager with auto-reconnect.
//!
//! Manages a WebSocket connection for real-time subscriptions (new blocks,
//! pending transactions, contract events). Handles disconnects with
//! exponential backoff reconnection.
//!
//! # Design
//!
//! One WS connection serves multiple subscription types. On disconnect,
//! the caller is responsible for re-establishing subscriptions after
//! calling [`WsManager::reconnect`]. The reconnect loop uses exponential
//! backoff capped at [`ReconnectConfig::max_backoff`].
//!
//! # Example
//!
//! ```rust,no_run
//! use perpcity_rust_sdk::transport::ws::{WsManager, ReconnectConfig};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let manager = WsManager::connect(
//!     "wss://base-rpc.publicnode.com",
//!     ReconnectConfig::default(),
//! ).await?;
//!
//! let mut headers = manager.subscribe_blocks().await?;
//! while let Some(header) = headers.recv().await {
//!     println!("new block: {}", header.number);
//! }
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;
use std::time::Duration;

use alloy::providers::{Provider, RootProvider};
use alloy::rpc::types::{Filter, Header, Log};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

/// Configuration for WebSocket reconnection behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectConfig {
    /// Initial backoff delay after first disconnect.
    pub initial_backoff: Duration,
    /// Maximum backoff delay.
    pub max_backoff: Duration,
    /// Multiplier for exponential backoff (applied each attempt).
    pub backoff_multiplier: u32,
    /// Maximum number of consecutive reconnect attempts (0 = unlimited).
    pub max_attempts: u32,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
            backoff_multiplier: 2,
            max_attempts: 0, // unlimited
        }
    }
}

/// WebSocket subscription manager.
///
/// Wraps an Alloy WebSocket-backed provider with auto-reconnect.
/// One connection serves all subscription types.
pub struct WsManager {
    url: String,
    config: ReconnectConfig,
    /// The underlying provider. Uses `RootProvider` (Ethereum network, no fillers)
    /// created via `ProviderBuilder::new().connect_ws(...)`.
    provider: Arc<RootProvider>,
}

impl std::fmt::Debug for WsManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsManager")
            .field("url", &self.url)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl WsManager {
    /// Connect to a WebSocket endpoint.
    ///
    /// Establishes the initial connection. If this fails, the error is returned
    /// immediately (no retry on initial connect — the caller should handle it).
    pub async fn connect(
        url: impl Into<String>,
        config: ReconnectConfig,
    ) -> crate::Result<Self> {
        let url = url.into();
        let ws_connect = alloy::providers::WsConnect::new(&url);
        // Build RPC client directly (no fillers — we manage nonce/gas ourselves)
        let rpc_client = alloy::rpc::client::ClientBuilder::default()
            .ws(ws_connect)
            .await?;
        let provider = RootProvider::new(rpc_client);

        Ok(Self {
            url,
            config,
            provider: Arc::new(provider),
        })
    }

    /// Subscribe to new block headers.
    ///
    /// Returns a channel receiver that yields blocks as they arrive.
    /// If the WebSocket disconnects, the sender is dropped and the receiver
    /// will return `None`.
    pub async fn subscribe_blocks(
        &self,
    ) -> crate::Result<mpsc::Receiver<Header>> {
        let (tx, rx) = mpsc::channel(64);
        let provider = Arc::clone(&self.provider);

        tokio::spawn(async move {
            let sub = match provider.subscribe_blocks().await {
                Ok(sub) => sub,
                Err(_) => return,
            };
            let mut stream = sub.into_stream();
            while let Some(block) = stream.next().await {
                if tx.send(block).await.is_err() {
                    break; // receiver dropped
                }
            }
        });

        Ok(rx)
    }

    /// Subscribe to contract event logs matching a filter.
    ///
    /// Returns a channel receiver that yields log entries.
    pub async fn subscribe_logs(
        &self,
        filter: Filter,
    ) -> crate::Result<mpsc::Receiver<Log>> {
        let (tx, rx) = mpsc::channel(256);
        let provider = Arc::clone(&self.provider);

        tokio::spawn(async move {
            let sub = match provider.subscribe_logs(&filter).await {
                Ok(sub) => sub,
                Err(_) => return,
            };
            let mut stream = sub.into_stream();
            while let Some(log) = stream.next().await {
                if tx.send(log).await.is_err() {
                    break;
                }
            }
        });

        Ok(rx)
    }

    /// Create a new WsManager by reconnecting to the same URL.
    ///
    /// Uses exponential backoff with the configured parameters.
    /// Returns `None` if max_attempts is exceeded.
    pub async fn reconnect(&self) -> Option<Self> {
        let mut delay = self.config.initial_backoff;
        let mut attempts = 0u32;

        loop {
            attempts += 1;
            if self.config.max_attempts > 0 && attempts > self.config.max_attempts {
                return None;
            }

            tokio::time::sleep(delay).await;

            match Self::connect(self.url.clone(), self.config).await {
                Ok(new_manager) => return Some(new_manager),
                Err(_) => {
                    // Exponential backoff
                    delay = (delay * self.config.backoff_multiplier).min(self.config.max_backoff);
                }
            }
        }
    }

    /// The WebSocket URL this manager is connected to.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Get a reference to the underlying provider for direct RPC calls.
    pub fn provider(&self) -> &RootProvider {
        &self.provider
    }
}

/// Compute the backoff delay for a given attempt number.
///
/// Uses exponential backoff: `min(initial * multiplier^attempt, max_backoff)`.
pub fn backoff_delay(config: &ReconnectConfig, attempt: u32) -> Duration {
    let multiplier = config
        .backoff_multiplier
        .checked_pow(attempt)
        .unwrap_or(u32::MAX);
    config
        .initial_backoff
        .saturating_mul(multiplier)
        .min(config.max_backoff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_delay_exponential() {
        let config = ReconnectConfig {
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(10),
            backoff_multiplier: 2,
            max_attempts: 0,
        };
        assert_eq!(backoff_delay(&config, 0), Duration::from_millis(100));
        assert_eq!(backoff_delay(&config, 1), Duration::from_millis(200));
        assert_eq!(backoff_delay(&config, 2), Duration::from_millis(400));
        assert_eq!(backoff_delay(&config, 3), Duration::from_millis(800));
    }

    #[test]
    fn backoff_delay_capped_at_max() {
        let config = ReconnectConfig {
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(500),
            backoff_multiplier: 2,
            max_attempts: 0,
        };
        assert_eq!(backoff_delay(&config, 5), Duration::from_millis(500));
        assert_eq!(backoff_delay(&config, 10), Duration::from_millis(500));
    }

    #[test]
    fn backoff_delay_handles_overflow() {
        let config = ReconnectConfig {
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            backoff_multiplier: 10,
            max_attempts: 0,
        };
        // 10^30 overflows u32, should clamp
        assert_eq!(backoff_delay(&config, 30), Duration::from_secs(60));
    }

    #[test]
    fn backoff_delay_multiplier_one_is_constant() {
        let config = ReconnectConfig {
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(10),
            backoff_multiplier: 1,
            max_attempts: 0,
        };
        assert_eq!(backoff_delay(&config, 0), Duration::from_millis(100));
        assert_eq!(backoff_delay(&config, 5), Duration::from_millis(100));
        assert_eq!(backoff_delay(&config, 100), Duration::from_millis(100));
    }
}
