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

/// What sending one request to an endpoint cost.
#[derive(Debug, PartialEq, Eq)]
pub enum Reserved {
    /// The circuit is closed: nothing was taken, nothing to give back.
    Freely,
    /// A half-open probe slot was taken, and has to be resolved — by the
    /// request's outcome, or by giving it back.
    Probe(ProbeSlot),
}

/// One half-open probe slot, named by the recovery cycle that gave it.
///
/// Single-use: it is neither `Clone` nor `Copy`, and every call that takes
/// it consumes it. Its owner resolves it once — a success closes the
/// circuit ([`EndpointHealth::record_success`]), a decline
/// ([`EndpointHealth::record_answered`]) or a request never sent
/// ([`EndpointHealth::release_probe`]) gives it back — and only within its
/// own cycle: an outcome that carries no slot, or a slot of an earlier
/// cycle, neither closes the circuit nor frees a probe now in flight.
#[derive(Debug, PartialEq, Eq)]
pub struct ProbeSlot {
    cycle: u64,
}

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
    /// The recovery cycle: counts the circuit's moves from open to
    /// half-open, so a probe slot names the cycle it belongs to.
    cycle: u64,

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
            cycle: 0,
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

    /// Whether [`Self::reserve`] would give a request the right to go at
    /// `now_ms`. A query: it takes no slot and changes no state.
    ///
    /// - **Closed**: always callable.
    /// - **Open**: callable once the recovery timeout has elapsed.
    /// - **HalfOpen**: callable while a probe slot is free.
    pub fn is_callable(&self, now_ms: u64) -> bool {
        match self.state {
            CircuitState::Closed => true,
            CircuitState::Open { since_ms } => self.recovered(since_ms, now_ms),
            CircuitState::HalfOpen { probes_in_flight } => {
                probes_in_flight < self.config.half_open_max_requests
            }
        }
    }

    /// Whether the recovery timeout of a circuit opened at `since_ms` has
    /// elapsed at `now_ms`.
    fn recovered(&self, since_ms: u64, now_ms: u64) -> bool {
        now_ms.saturating_sub(since_ms) >= self.config.recovery_timeout.as_millis() as u64
    }

    /// Take the right to send one request, and say whether that took a
    /// half-open probe slot.
    ///
    /// `None` when the endpoint is not callable. `Some(Reserved::Probe)`
    /// means a slot was taken and **must** be resolved — by recording the
    /// request's outcome with its slot, or by [`Self::release_probe`] if it
    /// is never sent. `Some(Reserved::Freely)` took nothing.
    ///
    /// Callers do not track this themselves: `EndpointPool` hands out a
    /// permit whose `Drop` releases an unresolved slot.
    pub fn reserve(&mut self, now_ms: u64) -> Option<Reserved> {
        match self.state {
            CircuitState::Closed => Some(Reserved::Freely),
            CircuitState::Open { since_ms } => {
                if self.recovered(since_ms, now_ms) {
                    // Recovery elapsed: a new cycle, whose first probe may go.
                    self.cycle += 1;
                    self.state = CircuitState::HalfOpen {
                        probes_in_flight: 1,
                    };
                    Some(Reserved::Probe(self.slot()))
                } else {
                    None
                }
            }
            CircuitState::HalfOpen { probes_in_flight } => {
                if probes_in_flight < self.config.half_open_max_requests {
                    self.state = CircuitState::HalfOpen {
                        probes_in_flight: probes_in_flight + 1,
                    };
                    Some(Reserved::Probe(self.slot()))
                } else {
                    None
                }
            }
        }
    }

    /// A slot of the current recovery cycle.
    fn slot(&self) -> ProbeSlot {
        ProbeSlot { cycle: self.cycle }
    }

    /// Give back `slot`, a half-open probe slot taken by [`Self::reserve`]
    /// for a request that was never sent, or whose outcome never arrived.
    ///
    /// Leaves the circuit half-open: the endpoint has still not shown
    /// whether it recovered. A slot of an earlier recovery cycle gives back
    /// nothing: its cycle ended when the circuit left half-open.
    pub fn release_probe(&mut self, slot: ProbeSlot) {
        if let CircuitState::HalfOpen { probes_in_flight } = self.state
            && slot.cycle == self.cycle
        {
            self.state = CircuitState::HalfOpen {
                probes_in_flight: probes_in_flight.saturating_sub(1),
            };
        }
    }

    /// Record that the endpoint answered by declining the request.
    ///
    /// Evidence that it is reachable and responsive, so the failure streak
    /// ends and a half-open probe gives its `slot` back — but not evidence
    /// that it can serve a request, so a half-open circuit stays half-open
    /// and the latency average is left alone. Without the slot release, a
    /// probe answered this way would hold its slot forever and the
    /// endpoint would never be probed again. A request sent while the
    /// circuit was closed holds no slot (`None`), so its answer gives back
    /// none of the probes in flight.
    pub fn record_answered(&mut self, slot: Option<ProbeSlot>) {
        self.consecutive_failures = 0;
        if let Some(slot) = slot {
            self.release_probe(slot);
        }
    }

    /// Record a successful request with its latency, and `slot`, the probe
    /// slot it held (`None` when it held none).
    ///
    /// Ends the failure streak and updates the latency average and the
    /// error rate. Only a probe of the current recovery cycle moves the
    /// circuit: its success closes a half-open circuit. A success that
    /// holds no slot, such as the late answer to a request sent while the
    /// circuit was closed, or a slot of an earlier cycle, leaves an open
    /// or half-open circuit as it is, so the probe in flight still decides.
    pub fn record_success(&mut self, latency_ns: u64, slot: Option<ProbeSlot>) {
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

        if let CircuitState::HalfOpen { .. } = self.state
            && slot.is_some_and(|slot| slot.cycle == self.cycle)
        {
            self.state = CircuitState::Closed;
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
        let h = EndpointHealth::new(default_config());
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
        assert!(matches!(h.reserve(33_000), Some(Reserved::Probe(_))));
        assert!(matches!(
            h.state(),
            CircuitState::HalfOpen {
                probes_in_flight: 1
            }
        ));
    }

    /// A probe the endpoint answers by declining has still been answered:
    /// its slot comes back, so the endpoint keeps being probed instead of
    /// sitting half-open with a slot it will never get back.
    #[test]
    fn a_declined_probe_returns_its_slot() {
        let config = CircuitBreakerConfig {
            half_open_max_requests: 1,
            ..default_config()
        };
        let mut h = EndpointHealth::new(config);
        for t in 1..=3 {
            h.record_failure(t * 1000);
        }
        let Some(Reserved::Probe(slot)) = h.reserve(33_000) else {
            panic!("recovery elapsed: one probe allowed");
        };
        assert!(!h.is_callable(33_001), "the probe is in flight");

        h.record_answered(Some(slot));
        assert!(
            h.is_callable(33_002),
            "the answered probe released its slot"
        );
        assert!(
            matches!(h.state(), CircuitState::HalfOpen { .. }),
            "declining is not proof the endpoint can serve the request"
        );
    }

    /// An answer breaks a failure streak, so unrelated failures either
    /// side of it do not add up toward the threshold.
    #[test]
    fn an_answer_ends_the_failure_streak() {
        let mut h = EndpointHealth::new(default_config());
        h.record_failure(1_000);
        h.record_failure(2_000);
        h.record_answered(None);
        assert_eq!(h.status().consecutive_failures, 0);
        h.record_failure(3_000);
        assert_eq!(h.state(), CircuitState::Closed, "one failure, not three");
    }

    /// A request sent while the circuit was closed holds no probe slot, so
    /// its late answer, which ends the failure streak, gives back none of
    /// the probes in flight.
    #[test]
    fn an_answer_without_a_slot_keeps_the_probe_in_flight() {
        let mut h = EndpointHealth::new(default_config());
        assert_eq!(h.reserve(0), Some(Reserved::Freely));
        for t in 1..=3 {
            h.record_failure(t * 1000);
        }
        assert!(matches!(h.reserve(33_000), Some(Reserved::Probe(_))));

        h.record_answered(None);
        assert!(!h.is_callable(33_001), "the probe still holds the slot");
    }

    /// A slot names its recovery cycle: once the circuit has opened again
    /// and a new probe is in flight, the old probe's slot gives back
    /// nothing.
    #[test]
    fn a_slot_of_an_earlier_cycle_releases_nothing() {
        let mut h = EndpointHealth::new(CircuitBreakerConfig {
            half_open_max_requests: 2,
            ..default_config()
        });
        for t in 1..=3 {
            h.record_failure(t * 1000);
        }
        let Some(Reserved::Probe(released)) = h.reserve(33_000) else {
            panic!("first cycle's probe");
        };
        let Some(Reserved::Probe(answered)) = h.reserve(33_001) else {
            panic!("first cycle's second probe");
        };
        h.record_failure(33_500);
        let Some(Reserved::Probe(new)) = h.reserve(64_000) else {
            panic!("second cycle's probe");
        };
        assert_ne!(released, new);
        let in_flight = |probes_in_flight| CircuitState::HalfOpen { probes_in_flight };

        h.release_probe(released);
        h.record_answered(Some(answered));
        assert_eq!(
            h.state(),
            in_flight(1),
            "the new probe still holds its slot"
        );

        h.release_probe(new);
        assert_eq!(h.state(), in_flight(0), "its owner gave the slot back");
    }

    /// A random walk over reservations and outcomes: while the circuit is
    /// half-open, its probes in flight are exactly the live requests that
    /// hold a slot of the current cycle, and never more than the limit.
    #[test]
    fn probes_in_flight_are_the_live_slots_of_the_cycle() {
        for (threshold, max) in [(1u32, 1u32), (1, 2), (3, 1), (2, 3)] {
            let mut h = EndpointHealth::new(CircuitBreakerConfig {
                failure_threshold: threshold,
                recovery_timeout: Duration::from_millis(50),
                half_open_max_requests: max,
            });
            let mut live: Vec<Option<ProbeSlot>> = Vec::new();
            let mut seed =
                0x9E37_79B9_7F4A_7C15u64 ^ ((u64::from(threshold) << 8) | u64::from(max));
            let mut next = move || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed
            };
            let (mut now, mut half_open, mut stale) = (0u64, 0u64, 0u64);
            for step in 0..200_000u64 {
                now += next() % 40;
                if next() % 10 < 5 || live.is_empty() {
                    match h.reserve(now) {
                        Some(Reserved::Freely) if live.len() < 6 => live.push(None),
                        Some(Reserved::Probe(slot)) => live.push(Some(slot)),
                        _ => {}
                    }
                } else {
                    let slot = live.swap_remove((next() % live.len() as u64) as usize);
                    match next() % 8 {
                        0 | 1 => h.record_success(1_000, slot),
                        2 | 3 => h.record_failure(now),
                        4 | 5 => h.record_answered(slot),
                        _ => {
                            if let Some(slot) = slot {
                                h.release_probe(slot);
                            }
                        }
                    }
                }
                if let CircuitState::HalfOpen { probes_in_flight } = h.state() {
                    half_open += 1;
                    let current = live.iter().flatten().filter(|s| s.cycle == h.cycle).count();
                    stale += u64::from(live.iter().flatten().any(|s| s.cycle != h.cycle));
                    assert_eq!(
                        probes_in_flight as usize, current,
                        "step {step} ({threshold}, {max})"
                    );
                    assert!(probes_in_flight <= max, "step {step} ({threshold}, {max})");
                }
            }
            assert!(
                half_open > 1_000 && stale > 10,
                "({threshold}, {max}): {half_open} {stale}"
            );
        }
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
        assert!(h.reserve(33_000).is_some()); // probe 1
        assert!(h.reserve(33_001).is_some()); // probe 2
        assert!(h.reserve(33_002).is_none()); // rejected — max probes reached
    }

    #[test]
    fn half_open_success_closes_circuit() {
        let mut h = EndpointHealth::new(default_config());
        for t in 1..=3 {
            h.record_failure(t * 1000);
        }
        let Some(Reserved::Probe(slot)) = h.reserve(33_000) else {
            panic!("recovery elapsed: one probe allowed");
        };
        h.record_success(500_000, Some(slot)); // probe succeeds
        assert_eq!(h.state(), CircuitState::Closed);
        assert!(h.is_callable(33_001));
    }

    /// Asking whether an endpoint is callable takes nothing: an open
    /// circuit whose recovery timeout has passed stays open, and a
    /// half-open one keeps its free slot, until a request reserves it.
    #[test]
    fn is_callable_is_a_query() {
        let mut h = EndpointHealth::new(default_config());
        for t in 1..=3 {
            h.record_failure(t * 1000);
        }
        assert!(!h.is_callable(32_999));
        for _ in 0..3 {
            assert!(h.is_callable(33_000));
        }
        assert!(matches!(h.state(), CircuitState::Open { since_ms: 3000 }));

        let Some(Reserved::Probe(_slot)) = h.reserve(33_000) else {
            panic!("recovery elapsed: one probe allowed");
        };
        assert!(!h.is_callable(33_001), "the only slot is held");
        assert_eq!(
            h.state(),
            CircuitState::HalfOpen {
                probes_in_flight: 1
            }
        );
    }

    /// A success that holds no slot — the late answer to a request sent
    /// while the circuit was closed — or a slot of an earlier cycle ends
    /// the failure streak and counts its latency, but neither closes an
    /// open circuit nor a half-open one whose probe is in flight: only the
    /// probe decides.
    #[test]
    fn only_a_probe_of_the_current_cycle_closes_the_circuit() {
        let mut h = EndpointHealth::new(default_config());
        assert_eq!(h.reserve(0), Some(Reserved::Freely));
        for t in 1..=3 {
            h.record_failure(t * 1000);
        }
        h.record_success(1_000, None);
        assert!(matches!(h.state(), CircuitState::Open { since_ms: 3000 }));
        assert_eq!(h.status().consecutive_failures, 0);
        assert_eq!(h.status().total_requests, 1);

        let Some(Reserved::Probe(old)) = h.reserve(33_000) else {
            panic!("first cycle's probe");
        };
        h.record_success(1_000, None);
        assert_eq!(
            h.state(),
            CircuitState::HalfOpen {
                probes_in_flight: 1
            },
            "a success with no slot leaves the probe in flight"
        );

        h.record_failure(33_500);
        let Some(Reserved::Probe(new)) = h.reserve(64_000) else {
            panic!("second cycle's probe");
        };
        h.record_success(1_000, Some(old));
        assert_eq!(
            h.state(),
            CircuitState::HalfOpen {
                probes_in_flight: 1
            },
            "a slot of an earlier cycle closes nothing"
        );

        h.record_success(1_000, Some(new));
        assert_eq!(h.state(), CircuitState::Closed);
    }

    /// A probe that succeeds after another probe of its cycle failed finds
    /// the circuit open again, and leaves it open.
    #[test]
    fn a_probe_that_succeeds_after_its_cycle_failed_closes_nothing() {
        let config = CircuitBreakerConfig {
            half_open_max_requests: 2,
            ..default_config()
        };
        let mut h = EndpointHealth::new(config);
        for t in 1..=3 {
            h.record_failure(t * 1000);
        }
        let Some(Reserved::Probe(first)) = h.reserve(33_000) else {
            panic!("first probe");
        };
        assert!(matches!(h.reserve(33_001), Some(Reserved::Probe(_))));
        h.record_failure(33_500);
        h.record_success(1_000, Some(first));
        assert!(matches!(h.state(), CircuitState::Open { since_ms: 33_500 }));
    }

    #[test]
    fn half_open_failure_reopens_circuit() {
        let mut h = EndpointHealth::new(default_config());
        for t in 1..=3 {
            h.record_failure(t * 1000);
        }
        assert!(h.reserve(33_000).is_some()); // transition to HalfOpen
        h.record_failure(33_500); // probe fails
        assert!(matches!(h.state(), CircuitState::Open { since_ms: 33_500 }));
    }

    // ── Latency tracking ─────────────────────────────────────────────

    #[test]
    fn first_sample_sets_latency_directly() {
        let mut h = EndpointHealth::new(default_config());
        h.record_success(10_000, None);
        assert_eq!(h.avg_latency_ns(), 10_000);
    }

    #[test]
    fn ema_converges_toward_new_value() {
        let mut h = EndpointHealth::new(default_config());
        h.record_success(10_000, None); // EMA = 10_000
        h.record_success(20_000, None); // EMA = 10_000*4/5 + 20_000/5 = 12_000
        assert_eq!(h.avg_latency_ns(), 12_000);
        h.record_success(20_000, None); // EMA = 12_000*4/5 + 20_000/5 = 13_600
        assert_eq!(h.avg_latency_ns(), 13_600);
    }

    #[test]
    fn success_resets_consecutive_failures() {
        let mut h = EndpointHealth::new(default_config());
        h.record_failure(1000);
        h.record_failure(2000);
        assert_eq!(h.status().consecutive_failures, 2);
        h.record_success(1000, None);
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
        h.record_success(1000, None); // rate = 0.1 * 0.9 = 0.09
        assert!((h.status().error_rate - 0.09).abs() < 1e-10);
    }

    // ── Status snapshot ──────────────────────────────────────────────

    #[test]
    fn status_reflects_current_state() {
        let mut h = EndpointHealth::new(default_config());
        h.record_success(5000, None);
        h.record_success(15000, None);
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
        assert!(h.reserve(3000).is_some());
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
        assert!(h.reserve(50).is_none()); // too early
        let Some(Reserved::Probe(slot)) = h.reserve(200) else {
            panic!("after 100ms cooldown");
        };
        assert!(matches!(h.state(), CircuitState::HalfOpen { .. }));

        // Probe succeeds → Closed
        h.record_success(1_000_000, Some(slot));
        assert_eq!(h.state(), CircuitState::Closed);
    }
}
