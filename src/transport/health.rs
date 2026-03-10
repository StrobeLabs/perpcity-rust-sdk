//! Per-endpoint health tracking with circuit breaker state machine.
//!
//! Each RPC endpoint is independently tracked with:
//! - Rolling latency window for performance-based selection
//! - Error rate tracking with exponential decay
//! - Three-state circuit breaker: Closed → Open → HalfOpen
//!
//! All time-dependent methods take explicit timestamps for deterministic testing.
//!
//! # Circuit breaker states
//!
//! ```text
//! ┌────────┐  failures >= threshold ┌──────┐  cooldown elapsed  ┌──────────┐
//! │ Closed │ ─────────────────────► │ Open │ ─────────────────► │ HalfOpen │
//! └────────┘                        └──────┘                    └──────────┘
//!     ▲                                 ▲                           │  │
//!     │                                 │  probe fails              │  │
//!     │                                 └───────────────────────────┘  │
//!     │            probe succeeds                                      │
//!     └─────────────────────────────────────────────────────────────── ┘
//! ```

use super::config::CircuitBreakerConfig;

/// Circuit breaker state for an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Healthy — all requests pass through.
    Closed,
    /// Dead — requests are rejected until the cooldown elapses.
    Open {
        /// When the circuit was opened (ms since epoch).
        since_ms: u64,
    },
    /// Probing — allowing limited requests to test recovery.
    HalfOpen {
        /// Number of probe requests currently in flight.
        probes_in_flight: u32,
    },
}

/// Health status of a single RPC endpoint.
///
/// Tracks latency (exponential moving average), consecutive failures,
/// error rate with exponential decay, and circuit breaker state.
#[derive(Debug)]
pub struct EndpointHealth {
    state: CircuitState,
    config: CircuitBreakerConfig,

    // Latency tracking (EMA with alpha ~0.2: new = old*4/5 + sample/5)
    avg_latency_ns: u64,
    total_requests: u64,

    // Error tracking
    consecutive_failures: u32,
    /// Exponential decay error rate in [0.0, 1.0].
    /// Decayed toward 0.0 on success, toward 1.0 on failure.
    /// Decay factor: `rate = rate * 0.9 + (is_error ? 0.1 : 0.0)`.
    error_rate: f64,
}

/// Snapshot of an endpoint's health for external inspection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EndpointStatus {
    /// Current circuit breaker state.
    pub state: CircuitState,
    /// Exponential moving average latency in nanoseconds.
    pub avg_latency_ns: u64,
    /// Total number of requests recorded.
    pub total_requests: u64,
    /// Smoothed error rate in [0.0, 1.0].
    pub error_rate: f64,
    /// Number of consecutive failures (resets on success).
    pub consecutive_failures: u32,
}

impl EndpointHealth {
    /// Create a new healthy endpoint tracker.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            state: CircuitState::Closed,
            config,
            avg_latency_ns: 0,
            total_requests: 0,
            consecutive_failures: 0,
            error_rate: 0.0,
        }
    }

    /// Current circuit breaker state.
    pub fn state(&self) -> CircuitState {
        self.state
    }

    /// Whether this endpoint can accept a request at the given time.
    ///
    /// - **Closed**: always callable.
    /// - **Open**: callable only if the recovery timeout has elapsed (transitions to HalfOpen).
    /// - **HalfOpen**: callable if probe slots are available.
    pub fn is_callable(&mut self, now_ms: u64) -> bool {
        match self.state {
            CircuitState::Closed => true,
            CircuitState::Open { since_ms } => {
                let elapsed = now_ms.saturating_sub(since_ms);
                if elapsed >= self.config.recovery_timeout.as_millis() as u64 {
                    // Transition to HalfOpen
                    self.state = CircuitState::HalfOpen {
                        probes_in_flight: 1,
                    };
                    true
                } else {
                    false
                }
            }
            CircuitState::HalfOpen { probes_in_flight } => {
                if probes_in_flight < self.config.half_open_max_requests {
                    self.state = CircuitState::HalfOpen {
                        probes_in_flight: probes_in_flight + 1,
                    };
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Record a successful request with its latency.
    ///
    /// Resets consecutive failure count and transitions HalfOpen → Closed.
    pub fn record_success(&mut self, latency_ns: u64) {
        // Update EMA latency (alpha ≈ 0.2)
        if self.total_requests == 0 {
            self.avg_latency_ns = latency_ns;
        } else {
            self.avg_latency_ns = self
                .avg_latency_ns
                .saturating_mul(4)
                .saturating_add(latency_ns)
                / 5;
        }
        self.total_requests += 1;
        self.consecutive_failures = 0;

        // Decay error rate toward 0
        self.error_rate *= 0.9;

        // State transition
        match self.state {
            CircuitState::HalfOpen { .. } => {
                // Probe succeeded → close the circuit
                self.state = CircuitState::Closed;
            }
            CircuitState::Open { .. } => {
                // Shouldn't happen (success implies a request got through),
                // but recover gracefully.
                self.state = CircuitState::Closed;
            }
            CircuitState::Closed => {}
        }
    }

    /// Record a failed request.
    ///
    /// Increments consecutive failures and may open the circuit.
    pub fn record_failure(&mut self, now_ms: u64) {
        self.consecutive_failures += 1;

        // Decay error rate toward 1
        self.error_rate = self.error_rate * 0.9 + 0.1;

        // State transitions
        match self.state {
            CircuitState::Closed => {
                if self.consecutive_failures >= self.config.failure_threshold {
                    self.state = CircuitState::Open { since_ms: now_ms };
                }
            }
            CircuitState::HalfOpen { .. } => {
                // Probe failed → back to Open
                self.state = CircuitState::Open { since_ms: now_ms };
            }
            CircuitState::Open { .. } => {
                // Already open, update timestamp
                self.state = CircuitState::Open { since_ms: now_ms };
            }
        }
    }

    /// Get the average latency for endpoint comparison.
    pub fn avg_latency_ns(&self) -> u64 {
        self.avg_latency_ns
    }

    /// Get a snapshot of this endpoint's health.
    pub fn status(&self) -> EndpointStatus {
        EndpointStatus {
            state: self.state,
            avg_latency_ns: self.avg_latency_ns,
            total_requests: self.total_requests,
            error_rate: self.error_rate,
            consecutive_failures: self.consecutive_failures,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn default_config() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            failure_threshold: 3,
            recovery_timeout: Duration::from_secs(30),
            half_open_max_requests: 1,
        }
    }

    // ── Circuit state transitions ────────────────────────────────────

    #[test]
    fn new_endpoint_is_closed_and_callable() {
        let mut h = EndpointHealth::new(default_config());
        assert_eq!(h.state(), CircuitState::Closed);
        assert!(h.is_callable(0));
    }

    #[test]
    fn opens_after_consecutive_failures_reach_threshold() {
        let mut h = EndpointHealth::new(default_config());
        h.record_failure(1000);
        h.record_failure(2000);
        assert_eq!(h.state(), CircuitState::Closed); // 2 < 3
        h.record_failure(3000);
        assert!(matches!(h.state(), CircuitState::Open { since_ms: 3000 }));
    }

    #[test]
    fn open_circuit_rejects_requests() {
        let mut h = EndpointHealth::new(default_config());
        for t in 1..=3 {
            h.record_failure(t * 1000);
        }
        // 5 seconds later — still within 30s recovery
        assert!(!h.is_callable(5000));
    }

    #[test]
    fn open_transitions_to_half_open_after_recovery_timeout() {
        let mut h = EndpointHealth::new(default_config());
        for t in 1..=3 {
            h.record_failure(t * 1000);
        }
        assert!(matches!(h.state(), CircuitState::Open { .. }));

        // After 30 seconds, should transition to HalfOpen
        assert!(h.is_callable(33_000));
        assert!(matches!(
            h.state(),
            CircuitState::HalfOpen {
                probes_in_flight: 1
            }
        ));
    }

    #[test]
    fn half_open_limits_probes() {
        let config = CircuitBreakerConfig {
            half_open_max_requests: 2,
            ..default_config()
        };
        let mut h = EndpointHealth::new(config);
        for t in 1..=3 {
            h.record_failure(t * 1000);
        }
        // Transition to HalfOpen
        assert!(h.is_callable(33_000)); // probe 1
        assert!(h.is_callable(33_001)); // probe 2
        assert!(!h.is_callable(33_002)); // rejected — max probes reached
    }

    #[test]
    fn half_open_success_closes_circuit() {
        let mut h = EndpointHealth::new(default_config());
        for t in 1..=3 {
            h.record_failure(t * 1000);
        }
        h.is_callable(33_000); // transition to HalfOpen
        h.record_success(500_000); // probe succeeds
        assert_eq!(h.state(), CircuitState::Closed);
        assert!(h.is_callable(33_001));
    }

    #[test]
    fn half_open_failure_reopens_circuit() {
        let mut h = EndpointHealth::new(default_config());
        for t in 1..=3 {
            h.record_failure(t * 1000);
        }
        h.is_callable(33_000); // transition to HalfOpen
        h.record_failure(33_500); // probe fails
        assert!(matches!(h.state(), CircuitState::Open { since_ms: 33_500 }));
    }

    // ── Latency tracking ─────────────────────────────────────────────

    #[test]
    fn first_sample_sets_latency_directly() {
        let mut h = EndpointHealth::new(default_config());
        h.record_success(10_000);
        assert_eq!(h.avg_latency_ns(), 10_000);
    }

    #[test]
    fn ema_converges_toward_new_value() {
        let mut h = EndpointHealth::new(default_config());
        h.record_success(10_000); // EMA = 10_000
        h.record_success(20_000); // EMA = 10_000*4/5 + 20_000/5 = 12_000
        assert_eq!(h.avg_latency_ns(), 12_000);
        h.record_success(20_000); // EMA = 12_000*4/5 + 20_000/5 = 13_600
        assert_eq!(h.avg_latency_ns(), 13_600);
    }

    #[test]
    fn success_resets_consecutive_failures() {
        let mut h = EndpointHealth::new(default_config());
        h.record_failure(1000);
        h.record_failure(2000);
        assert_eq!(h.status().consecutive_failures, 2);
        h.record_success(1000);
        assert_eq!(h.status().consecutive_failures, 0);
    }

    // ── Error rate decay ─────────────────────────────────────────────

    #[test]
    fn error_rate_increases_on_failures() {
        let mut h = EndpointHealth::new(default_config());
        assert_eq!(h.status().error_rate, 0.0);
        h.record_failure(1000);
        // rate = 0.0 * 0.9 + 0.1 = 0.1
        assert!((h.status().error_rate - 0.1).abs() < 1e-10);
        h.record_failure(2000);
        // rate = 0.1 * 0.9 + 0.1 = 0.19
        assert!((h.status().error_rate - 0.19).abs() < 1e-10);
    }

    #[test]
    fn error_rate_decays_on_success() {
        let mut h = EndpointHealth::new(default_config());
        h.record_failure(1000); // rate = 0.1
        h.record_success(1000); // rate = 0.1 * 0.9 = 0.09
        assert!((h.status().error_rate - 0.09).abs() < 1e-10);
    }

    // ── Status snapshot ──────────────────────────────────────────────

    #[test]
    fn status_reflects_current_state() {
        let mut h = EndpointHealth::new(default_config());
        h.record_success(5000);
        h.record_success(15000);
        let s = h.status();
        assert_eq!(s.state, CircuitState::Closed);
        assert_eq!(s.total_requests, 2);
        assert_eq!(s.consecutive_failures, 0);
        // EMA: 5000 first, then 5000*4/5 + 15000/5 = 7000
        assert_eq!(s.avg_latency_ns, 7000);
    }

    // ── Edge cases ───────────────────────────────────────────────────

    #[test]
    fn failure_threshold_one_opens_immediately() {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            ..default_config()
        };
        let mut h = EndpointHealth::new(config);
        h.record_failure(100);
        assert!(matches!(h.state(), CircuitState::Open { .. }));
    }

    #[test]
    fn recovery_timeout_zero_transitions_immediately() {
        let config = CircuitBreakerConfig {
            recovery_timeout: Duration::ZERO,
            ..default_config()
        };
        let mut h = EndpointHealth::new(config);
        for t in 1..=3 {
            h.record_failure(t * 1000);
        }
        // Even at the same timestamp, recovery_timeout=0 means immediate transition
        assert!(h.is_callable(3000));
        assert!(matches!(h.state(), CircuitState::HalfOpen { .. }));
    }

    #[test]
    fn multiple_failures_in_open_update_timestamp() {
        let mut h = EndpointHealth::new(default_config());
        for t in 1..=3 {
            h.record_failure(t * 1000);
        }
        // Additional failure while already open
        h.record_failure(5000);
        assert!(matches!(h.state(), CircuitState::Open { since_ms: 5000 }));
    }

    #[test]
    fn full_lifecycle_closed_open_halfopen_closed() {
        let config = CircuitBreakerConfig {
            failure_threshold: 2,
            recovery_timeout: Duration::from_millis(100),
            half_open_max_requests: 1,
        };
        let mut h = EndpointHealth::new(config);

        // Start closed
        assert_eq!(h.state(), CircuitState::Closed);

        // 2 failures → Open
        h.record_failure(10);
        h.record_failure(20);
        assert!(matches!(h.state(), CircuitState::Open { .. }));

        // Wait, then probe → HalfOpen
        assert!(!h.is_callable(50)); // too early
        assert!(h.is_callable(200)); // after 100ms cooldown
        assert!(matches!(h.state(), CircuitState::HalfOpen { .. }));

        // Probe succeeds → Closed
        h.record_success(1_000_000);
        assert_eq!(h.state(), CircuitState::Closed);
    }
}
