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
//! use perpcity_sdk::transport::config::TransportConfig;
//! use perpcity_sdk::transport::provider::HftTransport;
//! use alloy::providers::RootProvider;
//! use alloy::transports::BoxTransport;
//! use alloy::network::Ethereum;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let config = TransportConfig::builder()
//!     .shared_endpoint("https://base.g.alchemy.com/v2/KEY")
//!     .read_endpoint("https://base-rpc.publicnode.com")
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
        let old_state = h.state();
        h.record_success(latency_ns);
        let new_state = h.state();
        // Sync atomic mirrors (Relaxed is sufficient: no cross-field ordering needed,
        // eventual consistency is acceptable for endpoint selection heuristics)
        self.atomic_latency_ns
            .store(h.avg_latency_ns(), Ordering::Relaxed);
        self.atomic_state
            .store(pack_state(new_state), Ordering::Relaxed);
        if old_state != new_state {
            tracing::info!(
                endpoint = %self.url,
                from = ?old_state,
                to = ?new_state,
                "circuit breaker state changed"
            );
        }
    }

    /// Record a failed request. Updates Mutex state + atomic mirrors.
    #[inline]
    fn record_failure(&self, now_ms: u64) {
        let mut h = self.health.lock().unwrap();
        let old_state = h.state();
        h.record_failure(now_ms);
        let new_state = h.state();
        self.atomic_state
            .store(pack_state(new_state), Ordering::Relaxed);
        // Latency is not updated on failure (EMA stays the same).
        if old_state != new_state {
            tracing::warn!(
                endpoint = %self.url,
                from = ?old_state,
                to = ?new_state,
                consecutive_failures = h.status().consecutive_failures,
                "circuit breaker state changed"
            );
        }
    }
}

impl std::fmt::Debug for ManagedEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedEndpoint")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// A pool of RPC endpoints with health-aware selection.
///
/// Each pool owns its endpoints and round-robin counter, and operates
/// independently. Selection logic (round-robin, latency-based) is
/// encapsulated here — the [`Router`] delegates to the appropriate pool
/// based on whether the request is a read or write.
#[doc(hidden)]
#[derive(Debug)]
pub struct EndpointPool {
    endpoints: Vec<ManagedEndpoint>,
    round_robin: AtomicUsize,
}

impl EndpointPool {
    /// Build a pool from a list of endpoint URLs.
    pub fn from_urls(
        urls: &[String],
        cb_config: super::config::CircuitBreakerConfig,
    ) -> crate::Result<Self> {
        let endpoints = urls
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
                    health: Mutex::new(EndpointHealth::new(cb_config)),
                    url: url.clone(),
                    atomic_latency_ns: AtomicU64::new(0),
                    atomic_state: AtomicU64::new(TAG_CLOSED),
                })
            })
            .collect::<crate::Result<Vec<_>>>()?;

        Ok(Self {
            endpoints,
            round_robin: AtomicUsize::new(0),
        })
    }

    /// True if this pool has no endpoints.
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    /// Select the best endpoint index based on strategy.
    ///
    /// Returns `None` if all endpoints are unavailable.
    pub fn select(&self, strategy: Strategy, now_ms: u64) -> Option<usize> {
        match strategy {
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
    pub fn select_n(&self, n: usize, now_ms: u64) -> Vec<usize> {
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

    /// Number of endpoints in this pool.
    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    /// Record a successful request on endpoint `idx`.
    pub fn record_success(&self, idx: usize, latency_ns: u64) {
        self.endpoints[idx].record_success(latency_ns);
    }

    /// Record a failed request on endpoint `idx`.
    pub fn record_failure(&self, idx: usize, now_ms: u64) {
        self.endpoints[idx].record_failure(now_ms);
    }

    /// Clone the transport for endpoint `idx` (cheap, clones the inner Arc).
    fn transport(&self, idx: usize) -> alloy::transports::BoxTransport {
        self.endpoints[idx].transport.clone()
    }

    /// URL of endpoint `idx` (for diagnostics/tracing).
    fn url(&self, idx: usize) -> &str {
        &self.endpoints[idx].url
    }

    /// Number of endpoints currently in Closed (healthy) state.
    ///
    /// Lock-free: reads atomic state mirrors without taking any mutexes.
    pub fn healthy_count(&self) -> usize {
        self.endpoints
            .iter()
            .filter(|ep| ep.atomic_state.load(Ordering::Relaxed) & TAG_MASK == TAG_CLOSED)
            .count()
    }

    /// Health status of all endpoints in this pool.
    pub fn health_status(&self) -> Vec<EndpointStatus> {
        self.endpoints
            .iter()
            .map(|ep| ep.health.lock().unwrap().status())
            .collect()
    }

    /// URLs of all endpoints in this pool.
    pub fn endpoint_urls(&self) -> Vec<&str> {
        self.endpoints.iter().map(|ep| ep.url.as_str()).collect()
    }
}

/// Request router — manages endpoint pools and dispatches requests.
///
/// Holds three pools (shared, read, write) and routes each request to the
/// appropriate pool based on whether it is a read or write. If the dedicated
/// pool is empty or all its endpoints are unhealthy, the request falls back
/// to the shared pool.
#[derive(Debug)]
struct Router {
    shared: EndpointPool,
    read: EndpointPool,
    write: EndpointPool,
    strategy: Strategy,
    config: TransportConfig,
}

/// Multi-endpoint RPC transport with health-aware routing.
///
/// Implements `tower::Service<RequestPacket>` → Alloy `Transport` (blanket impl)
/// → usable with `RootProvider`.
///
/// Clone is cheap (Arc).
#[derive(Clone, Debug)]
pub struct HftTransport {
    router: Arc<Router>,
}

impl HftTransport {
    /// Create a new transport from configuration.
    ///
    /// Initializes one HTTP transport per configured endpoint. Each gets its own
    /// circuit breaker. This does NOT make any network calls — the transports
    /// connect lazily on first request.
    pub fn new(config: TransportConfig) -> crate::Result<Self> {
        let cb = config.circuit_breaker;
        let shared = EndpointPool::from_urls(&config.shared_endpoints, cb)?;
        let read = EndpointPool::from_urls(&config.read_endpoints, cb)?;
        let write = EndpointPool::from_urls(&config.write_endpoints, cb)?;

        Ok(Self {
            router: Arc::new(Router {
                shared,
                read,
                write,
                strategy: config.strategy,
                config,
            }),
        })
    }

    /// Get the health status of all endpoints across all pools.
    pub fn health_status(&self) -> Vec<EndpointStatus> {
        let r = &self.router;
        let mut out = r.shared.health_status();
        out.extend(r.read.health_status());
        out.extend(r.write.health_status());
        out
    }

    /// Number of endpoints currently in Closed (healthy) state across all pools.
    ///
    /// Lock-free: reads atomic state mirrors without taking any mutexes.
    pub fn healthy_count(&self) -> usize {
        let r = &self.router;
        r.shared.healthy_count() + r.read.healthy_count() + r.write.healthy_count()
    }

    /// URLs of all configured endpoints across all pools.
    pub fn endpoint_urls(&self) -> Vec<&str> {
        let r = &self.router;
        let mut out = r.shared.endpoint_urls();
        out.extend(r.read.endpoint_urls());
        out.extend(r.write.endpoint_urls());
        out
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
    matches!(method, "eth_sendRawTransaction" | "eth_sendTransaction")
}

// ── Pool-aware routing ─────────────────────────────────────────────

impl Router {
    /// Select the pool and endpoint index for a request.
    ///
    /// Tries the dedicated pool first (read or write), then falls back to
    /// the shared pool. Returns a reference to the chosen pool and the
    /// endpoint index within that pool, or `None` if all endpoints across
    /// both pools are unavailable.
    fn select_for(&self, is_write: bool, now_ms: u64) -> Option<(&EndpointPool, usize)> {
        let dedicated = if is_write { &self.write } else { &self.read };

        // Try dedicated pool first
        if !dedicated.is_empty() {
            if let Some(idx) = dedicated.select(self.strategy, now_ms) {
                return Some((dedicated, idx));
            }
        }

        // Fall back to shared pool
        self.shared
            .select(self.strategy, now_ms)
            .map(|idx| (&self.shared, idx))
    }

    /// Select the pool for hedged reads. Prefers the read pool if it has
    /// healthy endpoints, otherwise falls back to shared.
    fn read_pool(&self) -> &EndpointPool {
        if self.read.healthy_count() > 0 {
            &self.read
        } else {
            &self.shared
        }
    }

    /// Route a request through the best endpoint, with retry.
    ///
    /// Reads retry on any transport or RPC error. Writes only retry when the
    /// RPC node rejects the transaction before mempool inclusion (e.g. `-32003
    /// insufficient funds` from a stale read replica). A rejected tx never
    /// lands on-chain, so resending the same signed bytes is idempotent.
    async fn route_request(
        self: &Arc<Self>,
        req: RequestPacket,
    ) -> Result<ResponsePacket, TransportError> {
        let is_write = is_write_method(&req);
        let (max_attempts, base_delay) = if is_write {
            (
                1 + self.config.write_retry.max_retries,
                self.config.write_retry.base_delay,
            )
        } else {
            (
                1 + self.config.read_retry.max_retries,
                self.config.read_retry.base_delay,
            )
        };
        let timeout = self.config.request_timeout;

        // Handle hedged reads
        if !is_write && let Strategy::Hedged { fan_out } = self.strategy {
            let pool = self.read_pool();
            return self.hedged_request(pool, req, fan_out, timeout).await;
        }

        // Standard path: select endpoint from appropriate pool, try with retry
        let mut last_err = None;
        let now_ms = now_ms();

        for attempt in 0..max_attempts {
            let Some((pool, idx)) = self.select_for(is_write, now_ms) else {
                tracing::error!("all RPC endpoints unavailable (circuits open)");
                return Err(TransportError::local_usage_str(
                    "all RPC endpoints unavailable (circuits open)",
                ));
            };

            let start = Instant::now();
            let mut transport = pool.transport(idx);

            // Apply tower timeout
            let result = tokio::time::timeout(timeout, transport.call(req.clone())).await;

            match result {
                Ok(Ok(response)) => {
                    // For writes, check if the response is a pre-mempool rejection
                    // that is safe to retry (tx was never accepted).
                    if is_write && self.config.write_retry.is_retriable(&response) {
                        // Stale-replica rejections are not evidence of an
                        // unhealthy endpoint — don't touch the circuit breaker.
                        if attempt + 1 < max_attempts {
                            tracing::warn!(
                                attempt = attempt + 1,
                                max_attempts,
                                endpoint = %pool.url(idx),
                                error_code = response.first_error_code(),
                                "write rejected pre-mempool, retrying"
                            );
                        } else {
                            tracing::warn!(
                                endpoint = %pool.url(idx),
                                error_code = response.first_error_code(),
                                "write rejected after all retries exhausted"
                            );
                            return Ok(response);
                        }
                    } else {
                        let latency_ns = start.elapsed().as_nanos() as u64;
                        pool.record_success(idx, latency_ns);
                        return Ok(response);
                    }
                }
                Ok(Err(e)) => {
                    pool.record_failure(idx, now_ms);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        endpoint = %pool.url(idx),
                        error = %e,
                        is_write,
                        "transport error"
                    );
                    last_err = Some(e);
                }
                Err(_timeout) => {
                    pool.record_failure(idx, now_ms);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        endpoint = %pool.url(idx),
                        is_write,
                        "request timed out"
                    );
                    last_err = Some(TransportError::local_usage_str("request timed out"));
                }
            }

            // Backoff between retries (exponential: base * 2^attempt)
            if attempt + 1 < max_attempts {
                let delay = base_delay * 2u32.saturating_pow(attempt);
                tokio::time::sleep(delay).await;
            }
        }

        Err(last_err.unwrap_or_else(|| TransportError::local_usage_str("no endpoints available")))
    }

    /// Fan out a read request to multiple endpoints in a pool, return the
    /// fastest success.
    ///
    /// Uses [`JoinSet`] to properly cancel losing requests via `abort_all()`,
    /// saving RPC rate limits and network bandwidth. Health is recorded for all
    /// endpoints that complete before cancellation.
    async fn hedged_request(
        &self,
        pool: &EndpointPool,
        req: RequestPacket,
        fan_out: usize,
        timeout: std::time::Duration,
    ) -> Result<ResponsePacket, TransportError> {
        let now_ms = now_ms();
        let indices = pool.select_n(fan_out, now_ms);

        if indices.is_empty() {
            return Err(TransportError::local_usage_str(
                "all RPC endpoints unavailable (circuits open)",
            ));
        }

        // If only one endpoint is available, fall back to single request
        if indices.len() == 1 {
            let idx = indices[0];
            let start = Instant::now();
            let mut transport = pool.transport(idx);
            let result = tokio::time::timeout(timeout, transport.call(req)).await;

            return match result {
                Ok(Ok(resp)) => {
                    pool.record_success(idx, start.elapsed().as_nanos() as u64);
                    Ok(resp)
                }
                Ok(Err(e)) => {
                    pool.record_failure(idx, now_ms);
                    Err(e)
                }
                Err(_) => {
                    pool.record_failure(idx, now_ms);
                    Err(TransportError::local_usage_str("request timed out"))
                }
            };
        }

        // Fan out to multiple endpoints using JoinSet for proper cancellation
        let mut join_set = tokio::task::JoinSet::new();

        for &idx in &indices {
            let mut transport = pool.transport(idx);
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
                    pool.record_success(idx, latency_ns);
                    // Cancel remaining in-flight requests — saves RPC rate limits.
                    // JoinSet::drop also aborts, but explicit abort_all is clearer.
                    join_set.abort_all();
                    return Ok(response);
                }
                Ok((idx, Err(e), _start)) => {
                    pool.record_failure(idx, now_ms);
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

        Err(last_err
            .unwrap_or_else(|| TransportError::local_usage_str("all hedged requests failed")))
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
        let router = Arc::clone(&self.router);
        Box::pin(async move { router.route_request(req).await })
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
    fn new_transport_shared_only() {
        let config = TransportConfig::builder()
            .shared_endpoint("https://mainnet.base.org")
            .shared_endpoint("https://base-rpc.publicnode.com")
            .build()
            .unwrap();
        let transport = HftTransport::new(config).unwrap();
        assert_eq!(transport.healthy_count(), 2);
        assert_eq!(transport.endpoint_urls().len(), 2);
    }

    #[test]
    fn new_transport_read_write_split() {
        let config = TransportConfig::builder()
            .shared_endpoint("https://alchemy.example.com")
            .read_endpoint("https://public.example.com")
            .build()
            .unwrap();
        let transport = HftTransport::new(config).unwrap();
        assert_eq!(transport.healthy_count(), 2);
        assert_eq!(transport.endpoint_urls().len(), 2);
    }

    #[test]
    fn new_transport_invalid_url() {
        let config = TransportConfig::builder()
            .shared_endpoint("not a valid url")
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

    // ── EndpointPool selection tests ─────────────────────────────────

    #[test]
    fn pool_round_robin_selection() {
        let pool = EndpointPool::from_urls(
            &[
                "https://rpc1.example.com".into(),
                "https://rpc2.example.com".into(),
                "https://rpc3.example.com".into(),
            ],
            Default::default(),
        )
        .unwrap();

        let now = now_ms();
        let a = pool.select(Strategy::RoundRobin, now).unwrap();
        let b = pool.select(Strategy::RoundRobin, now).unwrap();
        let c = pool.select(Strategy::RoundRobin, now).unwrap();
        let d = pool.select(Strategy::RoundRobin, now).unwrap();

        // Should cycle through 0, 1, 2, 0
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_eq!(c, 2);
        assert_eq!(d, 0);
    }

    #[test]
    fn pool_latency_based_prefers_lower() {
        let pool = EndpointPool::from_urls(
            &[
                "https://rpc1.example.com".into(),
                "https://rpc2.example.com".into(),
            ],
            Default::default(),
        )
        .unwrap();

        pool.record_success(0, 10_000_000); // 10ms
        pool.record_success(1, 1_000_000); // 1ms

        let selected = pool.select(Strategy::LatencyBased, now_ms()).unwrap();
        assert_eq!(selected, 1); // lower latency
    }

    #[test]
    fn pool_skips_open_circuit() {
        let pool = EndpointPool::from_urls(
            &[
                "https://rpc1.example.com".into(),
                "https://rpc2.example.com".into(),
            ],
            Default::default(),
        )
        .unwrap();

        let now = now_ms();
        // Trip circuit breaker on endpoint 0
        pool.record_failure(0, now);
        pool.record_failure(0, now);
        pool.record_failure(0, now);

        let selected = pool.select(Strategy::LatencyBased, now).unwrap();
        assert_eq!(selected, 1); // only healthy endpoint
    }

    #[test]
    fn pool_select_n_ordered_by_latency() {
        let pool = EndpointPool::from_urls(
            &[
                "https://rpc1.example.com".into(),
                "https://rpc2.example.com".into(),
                "https://rpc3.example.com".into(),
            ],
            Default::default(),
        )
        .unwrap();

        pool.record_success(0, 5_000_000);
        pool.record_success(1, 1_000_000);
        pool.record_success(2, 3_000_000);

        let selected = pool.select_n(2, now_ms());
        assert_eq!(selected, vec![1, 2]); // ordered by latency, take 2
    }

    #[test]
    fn pool_all_circuits_open_returns_none() {
        let pool = EndpointPool::from_urls(
            &[
                "https://rpc1.example.com".into(),
                "https://rpc2.example.com".into(),
            ],
            Default::default(),
        )
        .unwrap();

        for idx in 0..pool.len() {
            for t in 1..=3 {
                pool.record_failure(idx, t * 1000);
            }
        }

        assert!(pool.select(Strategy::LatencyBased, 5000).is_none());
    }

    // ── Router fallback tests ────────────────────────────────────────

    #[test]
    fn router_read_uses_read_pool() {
        let config = TransportConfig::builder()
            .shared_endpoint("https://shared.example.com")
            .read_endpoint("https://read.example.com")
            .build()
            .unwrap();
        let transport = HftTransport::new(config).unwrap();
        let router = &transport.router;

        let (pool, _idx) = router.select_for(false, now_ms()).unwrap();
        // Should select from the read pool (1 endpoint), not shared
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.endpoint_urls()[0], "https://read.example.com");
    }

    #[test]
    fn router_write_uses_shared_when_no_write_pool() {
        let config = TransportConfig::builder()
            .shared_endpoint("https://shared.example.com")
            .read_endpoint("https://read.example.com")
            .build()
            .unwrap();
        let transport = HftTransport::new(config).unwrap();
        let router = &transport.router;

        let (pool, _idx) = router.select_for(true, now_ms()).unwrap();
        // No write pool → falls back to shared
        assert_eq!(pool.endpoint_urls()[0], "https://shared.example.com");
    }

    #[test]
    fn router_read_falls_back_to_shared() {
        let config = TransportConfig::builder()
            .shared_endpoint("https://shared.example.com")
            .read_endpoint("https://read.example.com")
            .build()
            .unwrap();
        let transport = HftTransport::new(config).unwrap();
        let router = &transport.router;

        // Trip the read pool's circuit breaker
        let now = now_ms();
        router.read.record_failure(0, now);
        router.read.record_failure(0, now);
        router.read.record_failure(0, now);

        // Read should fall back to shared
        let (pool, _idx) = router.select_for(false, now).unwrap();
        assert_eq!(pool.endpoint_urls()[0], "https://shared.example.com");
    }

    #[test]
    fn router_hedged_read_falls_back_to_shared() {
        let config = TransportConfig::builder()
            .shared_endpoint("https://shared.example.com")
            .read_endpoint("https://read.example.com")
            .strategy(Strategy::Hedged { fan_out: 2 })
            .build()
            .unwrap();
        let transport = HftTransport::new(config).unwrap();
        let router = &transport.router;

        // Trip the read pool's circuit breaker
        let now = now_ms();
        router.read.record_failure(0, now);
        router.read.record_failure(0, now);
        router.read.record_failure(0, now);

        // read_pool() should fall back to shared when read pool is unhealthy
        let pool = router.read_pool();
        assert_eq!(pool.endpoint_urls()[0], "https://shared.example.com");
    }

    // ── Lock-free verification tests ─────────────────────────────────

    #[test]
    fn atomic_state_reflects_mutations() {
        // This test verifies the lock-free atomic mirrors stay in sync
        // with the Mutex-protected health state. It accesses internal
        // fields because it's testing the internal consistency guarantee.
        let pool =
            EndpointPool::from_urls(&["https://rpc1.example.com".into()], Default::default())
                .unwrap();
        let ep = &pool.endpoints[0];

        // Initial state: Closed
        assert_eq!(
            ep.atomic_state.load(Ordering::Relaxed) & TAG_MASK,
            TAG_CLOSED
        );
        assert_eq!(ep.atomic_latency_ns.load(Ordering::Relaxed), 0);

        // After success: still Closed, latency updated
        pool.record_success(0, 5_000_000);
        assert_eq!(
            ep.atomic_state.load(Ordering::Relaxed) & TAG_MASK,
            TAG_CLOSED
        );
        assert_eq!(ep.atomic_latency_ns.load(Ordering::Relaxed), 5_000_000);

        // After 3 failures: state is Open
        pool.record_failure(0, 1000);
        pool.record_failure(0, 2000);
        pool.record_failure(0, 3000);
        assert_eq!(ep.atomic_state.load(Ordering::Relaxed) & TAG_MASK, TAG_OPEN);
    }

    #[test]
    fn healthy_count_across_pools() {
        let config = TransportConfig::builder()
            .shared_endpoint("https://shared1.example.com")
            .shared_endpoint("https://shared2.example.com")
            .read_endpoint("https://read.example.com")
            .build()
            .unwrap();
        let transport = HftTransport::new(config).unwrap();

        assert_eq!(transport.healthy_count(), 3);

        // Trip the read endpoint's circuit
        transport.router.read.record_failure(0, 1000);
        transport.router.read.record_failure(0, 2000);
        transport.router.read.record_failure(0, 3000);

        assert_eq!(transport.healthy_count(), 2);
    }

    #[test]
    fn pool_latency_fast_path_no_locks() {
        let pool = EndpointPool::from_urls(
            &[
                "https://rpc1.example.com".into(),
                "https://rpc2.example.com".into(),
            ],
            Default::default(),
        )
        .unwrap();

        pool.record_success(0, 10_000_000);
        pool.record_success(1, 2_000_000);

        // Multiple selections should consistently pick endpoint 1 (lower latency)
        for _ in 0..100 {
            assert_eq!(pool.select(Strategy::LatencyBased, 1000).unwrap(), 1);
        }
    }

    #[test]
    fn pool_select_n_fast_path_with_enough_closed() {
        let pool = EndpointPool::from_urls(
            &[
                "https://rpc1.example.com".into(),
                "https://rpc2.example.com".into(),
                "https://rpc3.example.com".into(),
            ],
            Default::default(),
        )
        .unwrap();

        pool.record_success(0, 8_000_000);
        pool.record_success(1, 2_000_000);
        pool.record_success(2, 5_000_000);

        let selected = pool.select_n(2, 1000);
        assert_eq!(selected, vec![1, 2]); // ordered by latency
    }
}
