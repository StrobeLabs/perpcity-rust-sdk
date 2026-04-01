//! Transport configuration with builder pattern.
//!
//! Configure multi-endpoint RPC transport with per-endpoint timeouts,
//! retry policies, circuit breaker thresholds, and routing strategies.
//!
//! Endpoints are organized into three pools:
//!
//! - **Shared** (`.shared_endpoint()`) — handles any request type and serves as
//!   fallback when dedicated read/write endpoints are unhealthy.
//! - **Read** (`.read_endpoint()`) — dedicated to read operations (`eth_call`,
//!   `eth_getBalance`, etc.). Falls back to the shared pool if all read endpoints
//!   are unhealthy.
//! - **Write** (`.write_endpoint()`) — dedicated to write operations
//!   (`eth_sendRawTransaction`). Falls back to the shared pool if all write
//!   endpoints are unhealthy.
//!
//! # Example
//!
//! ```
//! use perpcity_sdk::transport::config::{TransportConfig, Strategy};
//! use std::time::Duration;
//!
//! // Single shared endpoint (simplest setup, all requests go here)
//! let config = TransportConfig::builder()
//!     .shared_endpoint("https://base-rpc.publicnode.com")
//!     .build()
//!     .unwrap();
//!
//! // Read/write split: free public RPC for reads, paid for writes + fallback
//! let config = TransportConfig::builder()
//!     .shared_endpoint("https://base.g.alchemy.com/v2/KEY")
//!     .read_endpoint("https://base-rpc.publicnode.com")
//!     .ws_endpoint("wss://base-rpc.publicnode.com")
//!     .strategy(Strategy::LatencyBased)
//!     .request_timeout(Duration::from_millis(2000))
//!     .build()
//!     .unwrap();
//!
//! assert_eq!(config.shared_endpoints.len(), 1);
//! assert_eq!(config.read_endpoints.len(), 1);
//! assert!(config.ws_endpoint.is_some());
//! ```

use std::time::Duration;

use alloy::rpc::json_rpc::ResponsePacket;

use crate::errors::PerpCityError;

/// Endpoint selection strategy for routing RPC requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strategy {
    /// Cycle through healthy endpoints sequentially.
    RoundRobin,
    /// Pick the endpoint with the lowest observed latency.
    #[default]
    LatencyBased,
    /// Fan out reads to `fan_out` endpoints, take the fastest response.
    /// Writes always go to a single best endpoint.
    Hedged {
        /// Number of endpoints to fan out reads to.
        fan_out: usize,
    },
}

/// Circuit breaker configuration per endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CircuitBreakerConfig {
    /// Number of consecutive failures before opening the circuit.
    pub failure_threshold: u32,
    /// Time to wait in Open state before probing (HalfOpen).
    pub recovery_timeout: Duration,
    /// Maximum concurrent probe requests allowed in HalfOpen state.
    pub half_open_max_requests: u32,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            recovery_timeout: Duration::from_secs(30),
            half_open_max_requests: 1,
        }
    }
}

/// Retry configuration for read operations (any transport or RPC error).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadRetryConfig {
    /// Maximum number of retry attempts (0 = no retries, just the initial try).
    pub max_retries: u32,
    /// Base delay between retries. Scaled by 2^attempt for exponential backoff.
    pub base_delay: Duration,
}

impl Default for ReadRetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_delay: Duration::from_millis(100),
        }
    }
}

/// Retry configuration for write operations.
///
/// Writes are only retried when the RPC node *rejects* the transaction before
/// mempool inclusion (e.g. `-32003 insufficient funds` from a stale read
/// replica). A rejected tx never lands on-chain, so resending the same signed
/// bytes is safe and idempotent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteRetryConfig {
    /// Maximum number of retry attempts (0 = no retries, just the initial try).
    pub max_retries: u32,
    /// Base delay between retries. Scaled by 2^attempt for exponential backoff.
    pub base_delay: Duration,
}

impl Default for WriteRetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_millis(500),
        }
    }
}

impl WriteRetryConfig {
    /// Check if a JSON-RPC response is a pre-mempool rejection safe to retry.
    ///
    /// Any error response to `eth_sendRawTransaction` means the RPC node
    /// rejected the transaction before mempool inclusion — the signed bytes
    /// never landed on-chain, so resending them is always safe and idempotent.
    ///
    /// Rather than maintaining a fragile allow-list of specific error codes
    /// (e.g. `-32003`, `-32000` for "insufficient funds"), we retry on any
    /// error. The worst case for genuinely invalid transactions is a bounded
    /// delay (~1.75s) as retries exhaust harmlessly.
    pub fn is_retriable(&self, response: &ResponsePacket) -> bool {
        response.first_error_code().is_some()
    }
}

/// Complete transport configuration.
#[derive(Debug, Clone)]
pub struct TransportConfig {
    /// Shared HTTP RPC endpoints — handle any request type and serve as
    /// fallback when dedicated read/write endpoints are unhealthy.
    pub shared_endpoints: Vec<String>,
    /// Dedicated read endpoints. Read requests prefer these; falls back to
    /// shared endpoints if all read endpoints are unhealthy.
    pub read_endpoints: Vec<String>,
    /// Dedicated write endpoints. Write requests prefer these; falls back to
    /// shared endpoints if all write endpoints are unhealthy.
    pub write_endpoints: Vec<String>,
    /// Optional WebSocket endpoint URL for subscriptions.
    pub ws_endpoint: Option<String>,
    /// Per-request timeout.
    pub request_timeout: Duration,
    /// Endpoint selection strategy.
    pub strategy: Strategy,
    /// Circuit breaker settings (applied per endpoint).
    pub circuit_breaker: CircuitBreakerConfig,
    /// Retry settings for read operations.
    pub read_retry: ReadRetryConfig,
    /// Retry settings for write operations (pre-mempool rejections only).
    pub write_retry: WriteRetryConfig,
}

impl TransportConfig {
    /// Create a new builder for `TransportConfig`.
    pub fn builder() -> TransportConfigBuilder {
        TransportConfigBuilder::default()
    }
}

/// Builder for [`TransportConfig`].
#[derive(Debug, Clone)]
pub struct TransportConfigBuilder {
    shared_endpoints: Vec<String>,
    read_endpoints: Vec<String>,
    write_endpoints: Vec<String>,
    ws_endpoint: Option<String>,
    request_timeout: Duration,
    strategy: Strategy,
    circuit_breaker: CircuitBreakerConfig,
    read_retry: ReadRetryConfig,
    write_retry: WriteRetryConfig,
}

impl Default for TransportConfigBuilder {
    fn default() -> Self {
        Self {
            shared_endpoints: Vec::new(),
            read_endpoints: Vec::new(),
            write_endpoints: Vec::new(),
            ws_endpoint: None,
            request_timeout: Duration::from_secs(5),
            strategy: Strategy::default(),
            circuit_breaker: CircuitBreakerConfig::default(),
            read_retry: ReadRetryConfig::default(),
            write_retry: WriteRetryConfig::default(),
        }
    }
}

impl TransportConfigBuilder {
    /// Add a shared HTTP RPC endpoint.
    ///
    /// Shared endpoints handle any request type (reads and writes) and serve
    /// as the fallback when dedicated read or write endpoints are unhealthy.
    ///
    /// At least one endpoint must be configured across all pools.
    pub fn shared_endpoint(mut self, url: impl Into<String>) -> Self {
        self.shared_endpoints.push(url.into());
        self
    }

    /// Add a dedicated read endpoint.
    ///
    /// Read requests (`eth_call`, `eth_getBalance`, etc.) prefer these
    /// endpoints. If all read endpoints are unhealthy, reads fall back to
    /// the shared pool.
    pub fn read_endpoint(mut self, url: impl Into<String>) -> Self {
        self.read_endpoints.push(url.into());
        self
    }

    /// Add a dedicated write endpoint.
    ///
    /// Write requests (`eth_sendRawTransaction`) prefer these endpoints.
    /// If all write endpoints are unhealthy, writes fall back to the shared
    /// pool.
    pub fn write_endpoint(mut self, url: impl Into<String>) -> Self {
        self.write_endpoints.push(url.into());
        self
    }

    /// Set the WebSocket endpoint URL for subscriptions.
    pub fn ws_endpoint(mut self, url: impl Into<String>) -> Self {
        self.ws_endpoint = Some(url.into());
        self
    }

    /// Set the per-request timeout.
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Set the endpoint selection strategy.
    pub fn strategy(mut self, strategy: Strategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// Set the circuit breaker configuration.
    pub fn circuit_breaker(mut self, config: CircuitBreakerConfig) -> Self {
        self.circuit_breaker = config;
        self
    }

    /// Set the retry configuration for read operations.
    pub fn read_retry(mut self, config: ReadRetryConfig) -> Self {
        self.read_retry = config;
        self
    }

    /// Set the retry configuration for write operations.
    pub fn write_retry(mut self, config: WriteRetryConfig) -> Self {
        self.write_retry = config;
        self
    }

    /// Build the [`TransportConfig`].
    ///
    /// Returns an error if no endpoints are configured across any pool, or
    /// if writes have no reachable endpoint (write + shared pools both empty).
    pub fn build(self) -> crate::Result<TransportConfig> {
        let total =
            self.shared_endpoints.len() + self.read_endpoints.len() + self.write_endpoints.len();
        if total == 0 {
            return Err(PerpCityError::InvalidConfig {
                reason: "no endpoints configured".into(),
            });
        }
        if self.write_endpoints.is_empty() && self.shared_endpoints.is_empty() {
            return Err(PerpCityError::InvalidConfig {
                reason: "writes have no reachable endpoint: \
                         configure at least one shared or write endpoint"
                    .into(),
            });
        }
        if let Strategy::Hedged { fan_out } = self.strategy
            && fan_out < 2
        {
            return Err(PerpCityError::InvalidConfig {
                reason: "hedged strategy requires fan_out >= 2".into(),
            });
        }
        Ok(TransportConfig {
            shared_endpoints: self.shared_endpoints,
            read_endpoints: self.read_endpoints,
            write_endpoints: self.write_endpoints,
            ws_endpoint: self.ws_endpoint,
            request_timeout: self.request_timeout,
            strategy: self.strategy,
            circuit_breaker: self.circuit_breaker,
            read_retry: self.read_retry,
            write_retry: self.write_retry,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_defaults() {
        let config = TransportConfig::builder()
            .shared_endpoint("https://rpc1.example.com")
            .build()
            .unwrap();
        assert_eq!(config.shared_endpoints.len(), 1);
        assert!(config.read_endpoints.is_empty());
        assert!(config.write_endpoints.is_empty());
        assert!(config.ws_endpoint.is_none());
        assert_eq!(config.request_timeout, Duration::from_secs(5));
        assert_eq!(config.strategy, Strategy::LatencyBased);
        assert_eq!(config.circuit_breaker.failure_threshold, 3);
        assert_eq!(config.read_retry.max_retries, 2);
        assert_eq!(config.write_retry.max_retries, 3);
    }

    #[test]
    fn builder_all_options() {
        let config = TransportConfig::builder()
            .shared_endpoint("https://rpc1.example.com")
            .shared_endpoint("https://rpc2.example.com")
            .read_endpoint("https://read.example.com")
            .write_endpoint("https://write.example.com")
            .ws_endpoint("wss://ws.example.com")
            .request_timeout(Duration::from_millis(500))
            .strategy(Strategy::Hedged { fan_out: 3 })
            .circuit_breaker(CircuitBreakerConfig {
                failure_threshold: 5,
                recovery_timeout: Duration::from_secs(60),
                half_open_max_requests: 2,
            })
            .read_retry(ReadRetryConfig {
                max_retries: 5,
                base_delay: Duration::from_millis(50),
            })
            .write_retry(WriteRetryConfig {
                max_retries: 1,
                base_delay: Duration::from_millis(500),
            })
            .build()
            .unwrap();

        assert_eq!(config.shared_endpoints.len(), 2);
        assert_eq!(config.read_endpoints.len(), 1);
        assert_eq!(config.write_endpoints.len(), 1);
        assert_eq!(config.ws_endpoint.as_deref(), Some("wss://ws.example.com"));
        assert_eq!(config.request_timeout, Duration::from_millis(500));
        assert!(matches!(config.strategy, Strategy::Hedged { fan_out: 3 }));
        assert_eq!(config.circuit_breaker.failure_threshold, 5);
        assert_eq!(config.read_retry.max_retries, 5);
        assert_eq!(config.write_retry.max_retries, 1);
    }

    #[test]
    fn read_write_split() {
        let config = TransportConfig::builder()
            .shared_endpoint("https://alchemy.example.com")
            .read_endpoint("https://public.example.com")
            .build()
            .unwrap();
        assert_eq!(config.shared_endpoints.len(), 1);
        assert_eq!(config.read_endpoints.len(), 1);
        assert!(config.write_endpoints.is_empty());
    }

    #[test]
    fn no_endpoints_errors() {
        let result = TransportConfig::builder().build();
        assert!(result.is_err());
    }

    #[test]
    fn read_only_endpoints_errors() {
        // Only read endpoints, no shared or write — writes have nowhere to go
        let result = TransportConfig::builder()
            .read_endpoint("https://read.example.com")
            .build();
        assert!(result.is_err());
    }

    #[test]
    fn write_only_endpoints_ok() {
        // Only write endpoints — reads fall back to write pool? No, reads
        // fall back to shared which is empty. But writes work. This is a
        // valid (if unusual) config: the user only cares about writes.
        let result = TransportConfig::builder()
            .write_endpoint("https://write.example.com")
            .build();
        assert!(result.is_ok());
    }

    #[test]
    fn hedged_fan_out_one_errors() {
        let result = TransportConfig::builder()
            .shared_endpoint("https://rpc1.example.com")
            .strategy(Strategy::Hedged { fan_out: 1 })
            .build();
        assert!(result.is_err());
    }
}
