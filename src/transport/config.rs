//! Transport configuration with builder pattern.
//!
//! Configure multi-endpoint RPC transport with per-endpoint timeouts,
//! retry policies, circuit breaker thresholds, and routing strategies.
//!
//! # Example
//!
//! ```
//! use perpcity_rust_sdk::transport::config::{TransportConfig, Strategy};
//! use std::time::Duration;
//!
//! let config = TransportConfig::builder()
//!     .endpoint("https://mainnet.base.org")
//!     .endpoint("https://base-rpc.publicnode.com")
//!     .ws_endpoint("wss://base-rpc.publicnode.com")
//!     .strategy(Strategy::LatencyBased)
//!     .request_timeout(Duration::from_millis(2000))
//!     .build()
//!     .unwrap();
//!
//! assert_eq!(config.http_endpoints.len(), 2);
//! assert!(config.ws_endpoint.is_some());
//! ```

use std::time::Duration;

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
    Hedged { fan_out: usize },
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

/// Retry configuration for read operations. Writes are never retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryConfig {
    /// Maximum number of retry attempts (0 = no retries, just the initial try).
    pub max_retries: u32,
    /// Base delay between retries. Scaled by 2^attempt for exponential backoff.
    pub base_delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_delay: Duration::from_millis(100),
        }
    }
}

/// Complete transport configuration.
#[derive(Debug, Clone)]
pub struct TransportConfig {
    /// HTTP RPC endpoint URLs.
    pub http_endpoints: Vec<String>,
    /// Optional WebSocket endpoint URL for subscriptions.
    pub ws_endpoint: Option<String>,
    /// Per-request timeout.
    pub request_timeout: Duration,
    /// Endpoint selection strategy.
    pub strategy: Strategy,
    /// Circuit breaker settings (applied per endpoint).
    pub circuit_breaker: CircuitBreakerConfig,
    /// Retry settings for read operations.
    pub retry: RetryConfig,
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
    http_endpoints: Vec<String>,
    ws_endpoint: Option<String>,
    request_timeout: Duration,
    strategy: Strategy,
    circuit_breaker: CircuitBreakerConfig,
    retry: RetryConfig,
}

impl Default for TransportConfigBuilder {
    fn default() -> Self {
        Self {
            http_endpoints: Vec::new(),
            ws_endpoint: None,
            request_timeout: Duration::from_secs(5),
            strategy: Strategy::default(),
            circuit_breaker: CircuitBreakerConfig::default(),
            retry: RetryConfig::default(),
        }
    }
}

impl TransportConfigBuilder {
    /// Add an HTTP RPC endpoint URL.
    pub fn endpoint(mut self, url: impl Into<String>) -> Self {
        self.http_endpoints.push(url.into());
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
    pub fn retry(mut self, config: RetryConfig) -> Self {
        self.retry = config;
        self
    }

    /// Build the [`TransportConfig`].
    ///
    /// Returns an error if no HTTP endpoints are configured.
    pub fn build(self) -> crate::Result<TransportConfig> {
        if self.http_endpoints.is_empty() {
            return Err(PerpCityError::InvalidConfig {
                reason: "no HTTP endpoints configured".into(),
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
            http_endpoints: self.http_endpoints,
            ws_endpoint: self.ws_endpoint,
            request_timeout: self.request_timeout,
            strategy: self.strategy,
            circuit_breaker: self.circuit_breaker,
            retry: self.retry,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_defaults() {
        let config = TransportConfig::builder()
            .endpoint("https://rpc1.example.com")
            .build()
            .unwrap();
        assert_eq!(config.http_endpoints.len(), 1);
        assert!(config.ws_endpoint.is_none());
        assert_eq!(config.request_timeout, Duration::from_secs(5));
        assert_eq!(config.strategy, Strategy::LatencyBased);
        assert_eq!(config.circuit_breaker.failure_threshold, 3);
        assert_eq!(config.retry.max_retries, 2);
    }

    #[test]
    fn builder_all_options() {
        let config = TransportConfig::builder()
            .endpoint("https://rpc1.example.com")
            .endpoint("https://rpc2.example.com")
            .ws_endpoint("wss://ws.example.com")
            .request_timeout(Duration::from_millis(500))
            .strategy(Strategy::Hedged { fan_out: 3 })
            .circuit_breaker(CircuitBreakerConfig {
                failure_threshold: 5,
                recovery_timeout: Duration::from_secs(60),
                half_open_max_requests: 2,
            })
            .retry(RetryConfig {
                max_retries: 5,
                base_delay: Duration::from_millis(50),
            })
            .build()
            .unwrap();

        assert_eq!(config.http_endpoints.len(), 2);
        assert_eq!(config.ws_endpoint.as_deref(), Some("wss://ws.example.com"));
        assert_eq!(config.request_timeout, Duration::from_millis(500));
        assert!(matches!(config.strategy, Strategy::Hedged { fan_out: 3 }));
        assert_eq!(config.circuit_breaker.failure_threshold, 5);
        assert_eq!(config.retry.max_retries, 5);
    }

    #[test]
    fn no_endpoints_errors() {
        let result = TransportConfig::builder().build();
        assert!(result.is_err());
    }

    #[test]
    fn hedged_fan_out_one_errors() {
        let result = TransportConfig::builder()
            .endpoint("https://rpc1.example.com")
            .strategy(Strategy::Hedged { fan_out: 1 })
            .build();
        assert!(result.is_err());
    }

}
