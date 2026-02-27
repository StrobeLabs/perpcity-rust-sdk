//! Multi-endpoint RPC transport with health-aware routing.
//!
//! `HftTransport` implements [`tower::Service<RequestPacket>`], which makes it
//! a valid Alloy [`Transport`](alloy::transports::Transport) via the blanket
//! impl in `alloy-transport`. This means it can be used directly with
//! [`RootProvider`](alloy::providers::RootProvider) and all Alloy provider
//! methods.
//!
//! # Features
//!
//! - **Per-endpoint circuit breaker**: automatically routes around dead endpoints
//! - **Strategy-based selection**: round-robin, latency-based, or hedged reads
//! - **Read/write classification**: reads are retried on failure; writes never are
//! - **Hedged requests**: fan out reads to N endpoints, take the fastest response;
//!   losing requests are **cancelled** via `JoinSet::abort_all` to save RPC rate limits
//! - **Lock-free endpoint selection**: read path uses atomic mirrors, zero mutex
//!   contention in steady state (all endpoints healthy)
//! - **Tower integration**: composes with tower timeout and retry middleware
//!
//! # Example
//!
//! ```rust,no_run
//! use perpcity_rust_sdk::transport::config::TransportConfig;
//! use perpcity_rust_sdk::transport::provider::HftTransport;
//! use alloy::providers::RootProvider;
//! use alloy::transports::BoxTransport;
//! use alloy::network::Ethereum;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let config = TransportConfig::builder()
//!     .endpoint("https://mainnet.base.org")
//!     .endpoint("https://base-rpc.publicnode.com")
//!     .build()?;
//!
//! let transport = HftTransport::new(config)?;
//! let client = alloy::rpc::client::RpcClient::new(BoxTransport::new(transport), false);
//! let provider: RootProvider<Ethereum> = RootProvider::new(client);
//! # Ok(())
//! # }
//! ```

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use alloy::rpc::json_rpc::{RequestPacket, ResponsePacket};
use alloy::transports::{TransportError, TransportFut};
use tower::Service;

use super::config::{Strategy, TransportConfig};
use super::health::{CircuitState, EndpointHealth, EndpointStatus};

// ── Packed atomic state constants ────────────────────────────────────
//
// Circuit state is packed into a single AtomicU64 for lock-free reads:
//   bits[63:62] = state tag (00=Closed, 01=Open, 10=HalfOpen)
//   bits[61:0]  = since_ms (Open) or probes_in_flight (HalfOpen) or 0 (Closed)

const TAG_CLOSED: u64 = 0;
const TAG_OPEN: u64 = 1 << 62;
const TAG_HALFOPEN: u64 = 2 << 62;
const TAG_MASK: u64 = 3 << 62;

#[inline]
fn pack_state(state: CircuitState) -> u64 {
    match state {
        CircuitState::Closed => TAG_CLOSED,
        CircuitState::Open { since_ms } => TAG_OPEN | (since_ms & !TAG_MASK),
        CircuitState::HalfOpen { probes_in_flight } => TAG_HALFOPEN | (probes_in_flight as u64),
    }
}

/// A managed RPC endpoint: transport + health tracker + atomic mirrors.
struct ManagedEndpoint {
    /// The underlying Alloy boxed transport for this endpoint.
    transport: alloy::transports::BoxTransport,
    /// Per-endpoint health state (circuit breaker + latency). Protected by Mutex
    /// for mutations only; reads use atomic mirrors below.
    health: Mutex<EndpointHealth>,
    /// The endpoint URL (for diagnostics).
    url: String,
    // ── Lock-free mirrors (eventually consistent with Mutex state) ──
    // Updated after every health mutation. Reads never take locks.
    // Follows the evmap pattern: reads are lock-free, writes sync atomics.

    /// Atomic mirror of `EndpointHealth::avg_latency_ns`.
    /// Read by `select_latency_based` without locking.
    atomic_latency_ns: AtomicU64,
    /// Packed circuit state for lock-free reads.
    /// Read by `select_*` to filter callable endpoints without locking.
    atomic_state: AtomicU64,
}

impl ManagedEndpoint {
    /// Record a successful request. Updates Mutex state + atomic mirrors.
    #[inline]
    fn record_success(&self, latency_ns: u64) {
        let mut h = self.health.lock().unwrap();
        h.record_success(latency_ns);
        // Sync atomic mirrors (Relaxed is sufficient: no cross-field ordering needed,
        // eventual consistency is acceptable for endpoint selection heuristics)
        self.atomic_latency_ns
            .store(h.avg_latency_ns(), Ordering::Relaxed);
        self.atomic_state
            .store(pack_state(h.state()), Ordering::Relaxed);
    }

    /// Record a failed request. Updates Mutex state + atomic mirrors.
    #[inline]
    fn record_failure(&self, now_ms: u64) {
        let mut h = self.health.lock().unwrap();
        h.record_failure(now_ms);
        self.atomic_state
            .store(pack_state(h.state()), Ordering::Relaxed);
        // Latency is not updated on failure (EMA stays the same).
    }
}

impl std::fmt::Debug for ManagedEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedEndpoint")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// Shared inner state for the transport.
#[derive(Debug)]
struct TransportInner {
    endpoints: Vec<ManagedEndpoint>,
    strategy: Strategy,
    config: TransportConfig,
    round_robin: AtomicUsize,
}

/// Multi-endpoint RPC transport with health-aware routing.
///
/// Implements `tower::Service<RequestPacket>` → Alloy `Transport` (blanket impl)
/// → usable with `RootProvider`.
///
/// Clone is cheap (Arc).
#[derive(Clone, Debug)]
pub struct HftTransport {
    inner: Arc<TransportInner>,
}

impl HftTransport {
    /// Create a new transport from configuration.
    ///
    /// Initializes one HTTP transport per configured endpoint. Each gets its own
    /// circuit breaker. This does NOT make any network calls — the transports
    /// connect lazily on first request.
    pub fn new(config: TransportConfig) -> crate::Result<Self> {
        let endpoints = config
            .http_endpoints
            .iter()
            .map(|url| {
                let parsed: url::Url = url.parse().map_err(|e: url::ParseError| {
                    crate::PerpCityError::InvalidConfig {
                        reason: format!("invalid endpoint URL '{url}': {e}"),
                    }
                })?;
                let http = alloy::transports::http::Http::new(parsed);
                let boxed = alloy::transports::BoxTransport::new(http);
                Ok(ManagedEndpoint {
                    transport: boxed,
                    health: Mutex::new(EndpointHealth::new(config.circuit_breaker)),
                    url: url.clone(),
                    atomic_latency_ns: AtomicU64::new(0),
                    atomic_state: AtomicU64::new(TAG_CLOSED),
                })
            })
            .collect::<crate::Result<Vec<_>>>()?;

        Ok(Self {
            inner: Arc::new(TransportInner {
                endpoints,
                strategy: config.strategy,
                config,
                round_robin: AtomicUsize::new(0),
            }),
        })
    }

    /// Get the health status of all endpoints.
    pub fn health_status(&self) -> Vec<EndpointStatus> {
        self.inner
            .endpoints
            .iter()
            .map(|ep| ep.health.lock().unwrap().status())
            .collect()
    }

    /// Number of endpoints currently in Closed (healthy) state.
    ///
    /// Lock-free: reads atomic state mirrors without taking any mutexes.
    // healthy_count: lock-free via atomics (was N mutex locks)
    pub fn healthy_count(&self) -> usize {
        self.inner
            .endpoints
            .iter()
            .filter(|ep| ep.atomic_state.load(Ordering::Relaxed) & TAG_MASK == TAG_CLOSED)
            .count()
    }

    /// URLs of all configured endpoints.
    pub fn endpoint_urls(&self) -> Vec<&str> {
        self.inner
            .endpoints
            .iter()
            .map(|ep| ep.url.as_str())
            .collect()
    }

    // ── Benchmark accessors ─────────────────────────────────────────

    /// Select the best endpoint index based on the current strategy.
    ///
    /// Exposed for benchmarking endpoint selection latency.
    #[doc(hidden)]
    pub fn select_endpoint(&self, now_ms: u64) -> Option<usize> {
        self.inner.select_endpoint(now_ms)
    }

    /// Select up to `n` callable endpoints for hedged requests, ordered by latency.
    ///
    /// Exposed for benchmarking hedged fan-out overhead.
    #[doc(hidden)]
    pub fn select_n_endpoints(&self, n: usize, now_ms: u64) -> Vec<usize> {
        self.inner.select_n_endpoints(n, now_ms)
    }

    /// Record a success with latency on a specific endpoint.
    ///
    /// Exposed for benchmarking health recording.
    #[doc(hidden)]
    pub fn record_success(&self, idx: usize, latency_ns: u64) {
        self.inner.endpoints[idx].record_success(latency_ns);
    }

    /// Record a failure on a specific endpoint.
    ///
    /// Exposed for benchmarking health recording.
    #[doc(hidden)]
    pub fn record_failure(&self, idx: usize, now_ms: u64) {
        self.inner.endpoints[idx].record_failure(now_ms);
    }
}

// ── JSON-RPC method classification ──────────────────────────────────

/// Returns true if the JSON-RPC method is a write (state-changing) operation.
///
/// Write methods must NOT be retried — double-sends could cause double spends.
/// All other methods are treated as reads (safe to retry/hedge).
fn is_write_method(req: &RequestPacket) -> bool {
    match req {
        RequestPacket::Single(call) => is_write_method_name(call.method()),
        RequestPacket::Batch(calls) => calls.iter().any(|c| is_write_method_name(c.method())),
    }
}

fn is_write_method_name(method: &str) -> bool {
    matches!(
        method,
        "eth_sendRawTransaction" | "eth_sendTransaction"
    )
}

// ── Endpoint selection ──────────────────────────────────────────────

impl TransportInner {
    /// Select the best endpoint index based on the current strategy.
    ///
    /// Returns `None` if all endpoints are unavailable (circuit open + not yet
    /// past recovery timeout).
    fn select_endpoint(&self, now_ms: u64) -> Option<usize> {
        match self.strategy {
            Strategy::RoundRobin => self.select_round_robin(now_ms),
            Strategy::LatencyBased | Strategy::Hedged { .. } => self.select_latency_based(now_ms),
        }
    }

    /// Round-robin selection with lock-free fast path.
    ///
    /// Fast path: scan atomic state tags — if a Closed endpoint is found,
    /// return it immediately without locking. Only falls back to Mutex
    /// when all endpoints are non-Closed (rare: circuit breaker tripped).
    fn select_round_robin(&self, now_ms: u64) -> Option<usize> {
        let n = self.endpoints.len();
        let start = self.round_robin.fetch_add(1, Ordering::Relaxed);

        // Lock-free fast path: find first Closed endpoint in round-robin order
        for i in 0..n {
            let idx = (start + i) % n;
            if self.endpoints[idx].atomic_state.load(Ordering::Relaxed) & TAG_MASK == TAG_CLOSED {
                return Some(idx);
            }
        }

        // Slow path: all non-Closed, try is_callable (may transition Open→HalfOpen)
        for i in 0..n {
            let idx = (start + i) % n;
            let ep = &self.endpoints[idx];
            let mut h = ep.health.lock().unwrap();
            if h.is_callable(now_ms) {
                ep.atomic_state
                    .store(pack_state(h.state()), Ordering::Relaxed);
                return Some(idx);
            }
            ep.atomic_state
                .store(pack_state(h.state()), Ordering::Relaxed);
        }

        None
    }

    /// Latency-based selection with lock-free fast path.
    ///
    /// Fast path: scan atomic latency + state for all endpoints without locking.
    /// Among Closed endpoints, pick the one with lowest latency. This is the
    /// steady-state hot path — zero mutex contention.
    ///
    /// Slow path: no Closed endpoints available. Lock each non-Closed endpoint
    /// and call `is_callable()` which may transition Open→HalfOpen. Only entered
    /// when circuit breakers have tripped (error condition, rare).
    // select_latency_based: lock-free fast path via atomics (was N mutex locks)
    fn select_latency_based(&self, now_ms: u64) -> Option<usize> {
        // Lock-free fast path: find best Closed endpoint by latency
        let mut best_idx = None;
        let mut best_latency = u64::MAX;
        let mut any_non_closed = false;

        for (i, ep) in self.endpoints.iter().enumerate() {
            let state = ep.atomic_state.load(Ordering::Relaxed);
            if state & TAG_MASK == TAG_CLOSED {
                let lat = ep.atomic_latency_ns.load(Ordering::Relaxed);
                if lat < best_latency {
                    best_latency = lat;
                    best_idx = Some(i);
                }
            } else {
                any_non_closed = true;
            }
        }

        if best_idx.is_some() {
            return best_idx;
        }

        // Slow path: no Closed endpoints, try Open/HalfOpen with locks
        if any_non_closed {
            for (i, ep) in self.endpoints.iter().enumerate() {
                let mut h = ep.health.lock().unwrap();
                if h.is_callable(now_ms) {
                    let lat = h.avg_latency_ns();
                    // Sync atomics after potential state transition
                    ep.atomic_latency_ns
                        .store(h.avg_latency_ns(), Ordering::Relaxed);
                    ep.atomic_state
                        .store(pack_state(h.state()), Ordering::Relaxed);
                    if lat < best_latency {
                        best_latency = lat;
                        best_idx = Some(i);
                    }
                } else {
                    ep.atomic_state
                        .store(pack_state(h.state()), Ordering::Relaxed);
                }
            }
        }

        best_idx
    }

    /// Select up to `n` callable endpoints for hedged requests, ordered by latency.
    ///
    /// Uses a fixed-size stack buffer (max 16 endpoints) to avoid heap allocation
    /// in the common case.
    // select_n_endpoints: stack-allocated buffer + lock-free fast path
    fn select_n_endpoints(&self, n: usize, now_ms: u64) -> Vec<usize> {
        // Stack buffer avoids Vec allocation for up to 16 endpoints
        let mut candidates: [(usize, u64); 16] = [(0, u64::MAX); 16];
        let mut count = 0;
        let mut any_non_closed = false;

        // Lock-free fast path: collect Closed endpoints
        for (i, ep) in self.endpoints.iter().enumerate() {
            if count >= 16 {
                break;
            }
            let state = ep.atomic_state.load(Ordering::Relaxed);
            if state & TAG_MASK == TAG_CLOSED {
                let lat = ep.atomic_latency_ns.load(Ordering::Relaxed);
                candidates[count] = (i, lat);
                count += 1;
            } else {
                any_non_closed = true;
            }
        }

        // If we have enough Closed endpoints, sort and return top-n
        if count >= n {
            candidates[..count].sort_unstable_by_key(|&(_, lat)| lat);
            return candidates[..n].iter().map(|&(i, _)| i).collect();
        }

        // Slow path: not enough Closed, add recoverable Open/HalfOpen
        if any_non_closed {
            for (i, ep) in self.endpoints.iter().enumerate() {
                if count >= 16 {
                    break;
                }
                // Skip already-collected Closed endpoints
                let state = ep.atomic_state.load(Ordering::Relaxed);
                if state & TAG_MASK == TAG_CLOSED {
                    continue;
                }
                let mut h = ep.health.lock().unwrap();
                if h.is_callable(now_ms) {
                    let lat = h.avg_latency_ns();
                    ep.atomic_state
                        .store(pack_state(h.state()), Ordering::Relaxed);
                    candidates[count] = (i, lat);
                    count += 1;
                } else {
                    ep.atomic_state
                        .store(pack_state(h.state()), Ordering::Relaxed);
                }
            }
        }

        candidates[..count].sort_unstable_by_key(|&(_, lat)| lat);
        candidates[..count.min(n)].iter().map(|&(i, _)| i).collect()
    }

    /// Route a request through the best endpoint, with retry for reads.
    async fn route_request(
        self: &Arc<Self>,
        req: RequestPacket,
    ) -> Result<ResponsePacket, TransportError> {
        let is_write = is_write_method(&req);
        let max_attempts = if is_write {
            1
        } else {
            1 + self.config.retry.max_retries
        };
        let timeout = self.config.request_timeout;

        // Handle hedged reads
        if !is_write
            && let Strategy::Hedged { fan_out } = self.strategy
        {
            return self.hedged_request(req, fan_out, timeout).await;
        }

        // Standard path: select endpoint, try with retry
        let mut last_err = None;
        let now_ms = now_ms();

        for attempt in 0..max_attempts {
            let Some(idx) = self.select_endpoint(now_ms) else {
                return Err(TransportError::local_usage_str(
                    "all RPC endpoints unavailable (circuits open)",
                ));
            };

            let start = Instant::now();
            let mut transport = self.endpoints[idx].transport.clone();

            // Apply tower timeout
            let result = tokio::time::timeout(timeout, transport.call(req.clone())).await;

            match result {
                Ok(Ok(response)) => {
                    let latency_ns = start.elapsed().as_nanos() as u64;
                    self.endpoints[idx].record_success(latency_ns);
                    return Ok(response);
                }
                Ok(Err(e)) => {
                    self.endpoints[idx].record_failure(now_ms);
                    last_err = Some(e);
                }
                Err(_timeout) => {
                    self.endpoints[idx].record_failure(now_ms);
                    last_err = Some(TransportError::local_usage_str("request timed out"));
                }
            }

            // Backoff between retries (exponential: base * 2^attempt)
            if attempt + 1 < max_attempts {
                let delay = self.config.retry.base_delay * 2u32.saturating_pow(attempt);
                tokio::time::sleep(delay).await;
            }
        }

        Err(last_err.unwrap_or_else(|| {
            TransportError::local_usage_str("no endpoints available")
        }))
    }

    /// Fan out a read request to multiple endpoints, return the fastest success.
    ///
    /// Uses [`JoinSet`] to properly cancel losing requests via `abort_all()`,
    /// saving RPC rate limits and network bandwidth. Health is recorded for all
    /// endpoints that complete before cancellation.
    // hedged_request: JoinSet + abort_all (was mpsc + leaked tasks)
    async fn hedged_request(
        &self,
        req: RequestPacket,
        fan_out: usize,
        timeout: std::time::Duration,
    ) -> Result<ResponsePacket, TransportError> {
        let now_ms = now_ms();
        let indices = self.select_n_endpoints(fan_out, now_ms);

        if indices.is_empty() {
            return Err(TransportError::local_usage_str(
                "all RPC endpoints unavailable (circuits open)",
            ));
        }

        // If only one endpoint is available, fall back to single request
        if indices.len() == 1 {
            let idx = indices[0];
            let start = Instant::now();
            let mut transport = self.endpoints[idx].transport.clone();
            let result = tokio::time::timeout(timeout, transport.call(req)).await;

            return match result {
                Ok(Ok(resp)) => {
                    self.endpoints[idx].record_success(start.elapsed().as_nanos() as u64);
                    Ok(resp)
                }
                Ok(Err(e)) => {
                    self.endpoints[idx].record_failure(now_ms);
                    Err(e)
                }
                Err(_) => {
                    self.endpoints[idx].record_failure(now_ms);
                    Err(TransportError::local_usage_str("request timed out"))
                }
            };
        }

        // Fan out to multiple endpoints using JoinSet for proper cancellation
        let mut join_set = tokio::task::JoinSet::new();

        for &idx in &indices {
            let mut transport = self.endpoints[idx].transport.clone();
            let req_clone = req.clone();

            join_set.spawn(async move {
                let start = Instant::now();
                let result = tokio::time::timeout(timeout, transport.call(req_clone)).await;
                let result = match result {
                    Ok(r) => r,
                    Err(_) => Err(TransportError::local_usage_str("request timed out")),
                };
                (idx, result, start)
            });
        }

        let mut last_err = None;

        while let Some(join_result) = join_set.join_next().await {
            match join_result {
                Ok((idx, Ok(response), start)) => {
                    let latency_ns = start.elapsed().as_nanos() as u64;
                    self.endpoints[idx].record_success(latency_ns);
                    // Cancel remaining in-flight requests — saves RPC rate limits.
                    // JoinSet::drop also aborts, but explicit abort_all is clearer.
                    join_set.abort_all();
                    return Ok(response);
                }
                Ok((idx, Err(e), _start)) => {
                    self.endpoints[idx].record_failure(now_ms);
                    last_err = Some(e);
                }
                // Task was aborted (by our abort_all or JoinSet::drop) — expected
                Err(e) if e.is_cancelled() => {}
                // Task panicked — treat as failure
                Err(_) => {
                    last_err = Some(TransportError::local_usage_str(
                        "hedged request task panicked",
                    ));
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            TransportError::local_usage_str("all hedged requests failed")
        }))
    }
}

/// Get current time in milliseconds. Used for health tracking timestamps.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── tower::Service implementation ───────────────────────────────────
//
// This blanket-qualifies HftTransport as an Alloy Transport:
//   Service<RequestPacket, Response=ResponsePacket, Error=TransportError,
//           Future=TransportFut<'static>> + Clone + Send + Sync + 'static
//   → impl Transport for HftTransport
//   → BoxTransport::new(hft_transport) works
//   → RootProvider::new(hft_transport) works

impl Service<RequestPacket> for HftTransport {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // We're always ready to accept requests. Endpoint availability
        // is checked in `call` (fail-fast on route_request).
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { inner.route_request(req).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::config::TransportConfig;

    /// Helper to create a SerializedRequest for testing.
    fn make_request(method: &'static str, id: u64) -> alloy::rpc::json_rpc::SerializedRequest {
        use alloy::rpc::json_rpc::{Id, Request};
        let params = serde_json::value::RawValue::from_string("[]".to_string()).unwrap();
        Request::new(method, Id::Number(id), params)
            .serialize()
            .unwrap()
    }

    #[test]
    fn classify_write_methods() {
        let read = RequestPacket::Single(make_request("eth_getBlockByNumber", 1));
        assert!(!is_write_method(&read));

        let write = RequestPacket::Single(make_request("eth_sendRawTransaction", 2));
        assert!(is_write_method(&write));
    }

    #[test]
    fn classify_batch_with_write() {
        let batch = RequestPacket::Batch(vec![
            make_request("eth_getBalance", 1),
            make_request("eth_sendRawTransaction", 2),
        ]);
        assert!(is_write_method(&batch));
    }

    #[test]
    fn new_transport_valid_config() {
        let config = TransportConfig::builder()
            .endpoint("https://mainnet.base.org")
            .endpoint("https://base-rpc.publicnode.com")
            .build()
            .unwrap();
        let transport = HftTransport::new(config).unwrap();
        assert_eq!(transport.healthy_count(), 2);
        assert_eq!(transport.endpoint_urls().len(), 2);
    }

    #[test]
    fn new_transport_invalid_url() {
        let config = TransportConfig::builder()
            .endpoint("not a valid url")
            .build()
            .unwrap();
        let result = HftTransport::new(config);
        assert!(result.is_err());
    }

    #[test]
    fn transport_is_clone_send_sync() {
        fn assert_clone_send_sync<T: Clone + Send + Sync + 'static>() {}
        assert_clone_send_sync::<HftTransport>();
    }

    #[test]
    fn transport_implements_tower_service() {
        fn assert_service<T: tower::Service<RequestPacket>>() {}
        assert_service::<HftTransport>();
    }

    #[test]
    fn round_robin_selection() {
        let config = TransportConfig::builder()
            .endpoint("https://rpc1.example.com")
            .endpoint("https://rpc2.example.com")
            .endpoint("https://rpc3.example.com")
            .strategy(crate::transport::config::Strategy::RoundRobin)
            .build()
            .unwrap();
        let transport = HftTransport::new(config).unwrap();
        let inner = &transport.inner;

        let now = now_ms();
        let a = inner.select_endpoint(now).unwrap();
        let b = inner.select_endpoint(now).unwrap();
        let c = inner.select_endpoint(now).unwrap();
        let d = inner.select_endpoint(now).unwrap();

        // Should cycle through 0, 1, 2, 0
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_eq!(c, 2);
        assert_eq!(d, 0);
    }

    #[test]
    fn latency_based_selection_prefers_lower_latency() {
        let config = TransportConfig::builder()
            .endpoint("https://rpc1.example.com") // idx 0
            .endpoint("https://rpc2.example.com") // idx 1
            .strategy(crate::transport::config::Strategy::LatencyBased)
            .build()
            .unwrap();
        let transport = HftTransport::new(config).unwrap();
        let inner = &transport.inner;

        // Use the centralized method that syncs atomics
        inner.endpoints[0].record_success(10_000_000); // 10ms
        inner.endpoints[1].record_success(1_000_000); // 1ms

        let now = now_ms();
        let selected = inner.select_endpoint(now).unwrap();
        assert_eq!(selected, 1); // lower latency
    }

    #[test]
    fn selection_skips_open_circuit() {
        let config = TransportConfig::builder()
            .endpoint("https://rpc1.example.com")
            .endpoint("https://rpc2.example.com")
            .strategy(crate::transport::config::Strategy::LatencyBased)
            .build()
            .unwrap();
        let transport = HftTransport::new(config).unwrap();
        let inner = &transport.inner;

        let now = now_ms();
        // Trip circuit breaker on endpoint 0 using centralized method
        inner.endpoints[0].record_failure(now);
        inner.endpoints[0].record_failure(now);
        inner.endpoints[0].record_failure(now);

        // Select at `now` — still within 30s recovery timeout
        let selected = inner.select_endpoint(now).unwrap();
        assert_eq!(selected, 1); // only healthy endpoint
    }

    #[test]
    fn select_n_endpoints_ordered_by_latency() {
        let config = TransportConfig::builder()
            .endpoint("https://rpc1.example.com") // idx 0
            .endpoint("https://rpc2.example.com") // idx 1
            .endpoint("https://rpc3.example.com") // idx 2
            .build()
            .unwrap();
        let transport = HftTransport::new(config).unwrap();
        let inner = &transport.inner;

        // Set latencies via centralized method (syncs atomics)
        inner.endpoints[0].record_success(5_000_000);
        inner.endpoints[1].record_success(1_000_000);
        inner.endpoints[2].record_success(3_000_000);

        let now = now_ms();
        let selected = inner.select_n_endpoints(2, now);
        assert_eq!(selected, vec![1, 2]); // ordered by latency, take 2
    }

    #[test]
    fn all_circuits_open_returns_none() {
        let config = TransportConfig::builder()
            .endpoint("https://rpc1.example.com")
            .endpoint("https://rpc2.example.com")
            .build()
            .unwrap();
        let transport = HftTransport::new(config).unwrap();
        let inner = &transport.inner;

        // Trip both circuit breakers
        for ep in &inner.endpoints {
            for t in 1..=3 {
                ep.record_failure(t * 1000);
            }
        }

        let now_ms = 5000; // within recovery timeout
        assert!(inner.select_endpoint(now_ms).is_none());
    }

    // ── Lock-free verification tests ─────────────────────────────────

    #[test]
    fn atomic_state_reflects_mutations() {
        let config = TransportConfig::builder()
            .endpoint("https://rpc1.example.com")
            .build()
            .unwrap();
        let transport = HftTransport::new(config).unwrap();
        let inner = &transport.inner;
        let ep = &inner.endpoints[0];

        // Initial state: Closed
        assert_eq!(ep.atomic_state.load(Ordering::Relaxed) & TAG_MASK, TAG_CLOSED);
        assert_eq!(ep.atomic_latency_ns.load(Ordering::Relaxed), 0);

        // After success: still Closed, latency updated
        ep.record_success(5_000_000);
        assert_eq!(ep.atomic_state.load(Ordering::Relaxed) & TAG_MASK, TAG_CLOSED);
        assert_eq!(ep.atomic_latency_ns.load(Ordering::Relaxed), 5_000_000);

        // After 3 failures: state is Open
        ep.record_failure(1000);
        ep.record_failure(2000);
        ep.record_failure(3000);
        assert_eq!(ep.atomic_state.load(Ordering::Relaxed) & TAG_MASK, TAG_OPEN);
    }

    #[test]
    fn healthy_count_is_lock_free() {
        let config = TransportConfig::builder()
            .endpoint("https://rpc1.example.com")
            .endpoint("https://rpc2.example.com")
            .endpoint("https://rpc3.example.com")
            .build()
            .unwrap();
        let transport = HftTransport::new(config).unwrap();

        assert_eq!(transport.healthy_count(), 3);

        // Trip one circuit
        transport.record_failure(0, 1000);
        transport.record_failure(0, 2000);
        transport.record_failure(0, 3000);

        assert_eq!(transport.healthy_count(), 2);
    }

    #[test]
    fn latency_based_fast_path_no_locks() {
        // Verify that with all Closed endpoints, select_latency_based
        // uses the atomic fast path (doesn't modify state)
        let config = TransportConfig::builder()
            .endpoint("https://rpc1.example.com")
            .endpoint("https://rpc2.example.com")
            .strategy(Strategy::LatencyBased)
            .build()
            .unwrap();
        let transport = HftTransport::new(config).unwrap();
        let inner = &transport.inner;

        inner.endpoints[0].record_success(10_000_000);
        inner.endpoints[1].record_success(2_000_000);

        // Multiple selections should consistently pick endpoint 1 (lower latency)
        for _ in 0..100 {
            assert_eq!(inner.select_endpoint(1000).unwrap(), 1);
        }
    }

    #[test]
    fn select_n_fast_path_with_enough_closed() {
        let config = TransportConfig::builder()
            .endpoint("https://rpc1.example.com")
            .endpoint("https://rpc2.example.com")
            .endpoint("https://rpc3.example.com")
            .build()
            .unwrap();
        let transport = HftTransport::new(config).unwrap();
        let inner = &transport.inner;

        inner.endpoints[0].record_success(8_000_000);
        inner.endpoints[1].record_success(2_000_000);
        inner.endpoints[2].record_success(5_000_000);

        // All Closed, requesting 2 — should use fast path
        let selected = inner.select_n_endpoints(2, 1000);
        assert_eq!(selected, vec![1, 2]); // ordered by latency
    }
}
