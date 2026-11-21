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
//! - **A trading transport**: it sends reads and signed raw transactions
//!   only. A method on an allowlist of reads without side effects
//!   (`eth_call`, `eth_getLogs`, `eth_blockNumber` and the other reads the
//!   SDK sends), alone or in a batch of reads, is retried and hedged. A
//!   read is retried when the endpoint failed to answer, never when it
//!   declined the request. The only write is a single
//!   `eth_sendRawTransaction`, retried as the same signed bytes after an
//!   error answer and after no answer: the bytes are one transaction, so a
//!   resend cannot make a second, but the outcome of an earlier attempt is
//!   unknown, so a caller reconciling a send should trust the receipt
//!   rather than the error
//!   ([`TransactionError::tx_hash`](crate::errors::TransactionError::tx_hash)).
//!   Every other method (`eth_sendTransaction`, a synchronous or
//!   conditional send, `personal_*`, `eth_subscribe`, an unknown method)
//!   and any batch that holds one is refused before it is sent, with
//!   `RpcError::LocalUsageError`, whose message names the method:
//!   `HftTransport sends only single signed eth_sendRawTransaction writes;
//!   <method> is not supported`
//! - **Health from answers**: an answer under another id or of another
//!   shape, a body that is no JSON-RPC response, and a rate limit anywhere
//!   in a read's answer count against the endpoint; a read that got no
//!   answer under its id is asked again, and a hedged read waits for one
//! - **Body checked against metadata**: every check reads a call's method
//!   and id from alloy's metadata, so a call whose serialized JSON body
//!   holds another method or id is refused with
//!   `RpcError::LocalUsageError` before it is sent
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

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use alloy::primitives::B256;
use alloy::rpc::json_rpc::{RequestPacket, ResponsePacket, ResponsePayload, SerializedRequest};
use alloy::transports::{TransportError, TransportErrorKind, TransportFut};
use serde::Deserializer as _;
use serde::de::Error as _;
use tower::Service;

use super::config::{Strategy, TransportConfig};
use super::fault::{Fault, fault, is_rate_limit, is_rejection};
use super::health::{CircuitState, EndpointHealth, EndpointStatus, ProbeSlot, Reserved};

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
    /// The endpoint URL. Holds credentials for hosted providers: never log it,
    /// log `label` instead.
    url: String,
    /// Host-only form of `url`, safe for logs and `Debug`.
    label: String,
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
    /// Record a successful request, which held `slot`. Updates Mutex
    /// state + atomic mirrors.
    #[inline]
    fn record_success(&self, latency_ns: u64, slot: Option<ProbeSlot>) {
        let mut h = self.health.lock().unwrap();
        let old_state = h.state();
        h.record_success(latency_ns, slot);
        let new_state = h.state();
        // Sync atomic mirrors (Relaxed is sufficient: no cross-field ordering needed,
        // eventual consistency is acceptable for endpoint selection heuristics)
        self.atomic_latency_ns
            .store(h.avg_latency_ns(), Ordering::Relaxed);
        self.atomic_state
            .store(pack_state(new_state), Ordering::Relaxed);
        if old_state != new_state {
            tracing::debug!(
                endpoint = %self.label,
                from = ?old_state,
                to = ?new_state,
                "circuit breaker state changed"
            );
        }
    }

    /// Give back a half-open probe slot whose request was never sent.
    /// Updates Mutex state + atomic mirrors.
    #[inline]
    fn release_probe(&self, slot: ProbeSlot) {
        let mut h = self.health.lock().unwrap();
        h.release_probe(slot);
        self.atomic_state
            .store(pack_state(h.state()), Ordering::Relaxed);
    }

    /// Record an answer that declined the request, which held `slot`.
    /// Updates Mutex state + atomic mirrors.
    #[inline]
    fn record_answered(&self, slot: Option<ProbeSlot>) {
        let mut h = self.health.lock().unwrap();
        h.record_answered(slot);
        self.atomic_state
            .store(pack_state(h.state()), Ordering::Relaxed);
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
                endpoint = %self.label,
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
            .field("endpoint", &self.label)
            .finish_non_exhaustive()
    }
}

/// The right to send one request to an endpoint, holding a half-open
/// probe slot until the request's outcome is recorded.
///
/// Selecting an endpoint can reserve a probe slot, and a slot that is
/// never given back stops the endpoint being probed again — with the
/// default budget of one, forever. So the reservation is a value: drop it
/// without sending, or drop it because the request was cancelled, and the
/// slot comes back. Recording the request's outcome
/// ([`EndpointPool::record_success`], [`EndpointPool::record_error`])
/// consumes the permit and settles the slot with it. The permit names its
/// slot and the recovery cycle that gave it, so it gives back or closes
/// with its own slot and no other request's, and a permit taken while the
/// circuit was closed holds none.
#[must_use = "dropping the permit releases the endpoint's probe slot; that is usually what you want, but say so"]
#[derive(Debug)]
pub struct ProbePermit<'a> {
    /// `None` once resolved, or when the circuit was closed and no slot
    /// was taken.
    held: Option<(&'a ManagedEndpoint, ProbeSlot)>,
}

impl ProbePermit<'_> {
    /// A permit over a closed circuit: nothing was reserved, so nothing
    /// has to be given back. Cheap enough for the lock-free path.
    fn free() -> Self {
        Self { held: None }
    }

    /// The probe slot this permit holds on `endpoint`, taken out for the
    /// request's outcome to settle; `None` when it holds none there. A
    /// slot on another endpoint is given back to it when the permit drops.
    fn into_slot(mut self, endpoint: &ManagedEndpoint) -> Option<ProbeSlot> {
        match self.held.take() {
            Some((held, slot)) if std::ptr::eq(held, endpoint) => Some(slot),
            other => {
                self.held = other;
                None
            }
        }
    }
}

impl Drop for ProbePermit<'_> {
    fn drop(&mut self) {
        if let Some((endpoint, slot)) = self.held.take() {
            endpoint.release_probe(slot);
        }
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
                    crate::errors::ValidationError::InvalidConfig {
                        reason: format!(
                            "invalid endpoint URL for host '{}': {e}",
                            super::redact_url(url)
                        ),
                    }
                })?;
                let http = super::redact::UrlStrippingHttp::new(parsed);
                let boxed = alloy::transports::BoxTransport::new(http);
                Ok(ManagedEndpoint {
                    transport: boxed,
                    health: Mutex::new(EndpointHealth::new(cb_config)),
                    url: url.clone(),
                    label: super::redact_url(url),
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

    /// Select the best endpoint based on strategy, with the right to send
    /// it one request.
    ///
    /// Returns `None` if all endpoints are unavailable. The permit must
    /// outlive the request: dropping it gives back any probe slot the
    /// selection reserved.
    pub fn select(&self, strategy: Strategy, now_ms: u64) -> Option<(usize, ProbePermit<'_>)> {
        match strategy {
            Strategy::RoundRobin => self.select_round_robin(now_ms),
            Strategy::LatencyBased | Strategy::Hedged { .. } => self.select_latency_based(now_ms),
        }
    }

    /// Take the right to send one request to endpoint `idx`, or `None` if
    /// its circuit will not allow one.
    pub fn reserve(&self, idx: usize, now_ms: u64) -> Option<ProbePermit<'_>> {
        let endpoint = &self.endpoints[idx];
        let mut health = endpoint.health.lock().unwrap();
        let reserved = health.reserve(now_ms);
        endpoint
            .atomic_state
            .store(pack_state(health.state()), Ordering::Relaxed);
        endpoint
            .atomic_latency_ns
            .store(health.avg_latency_ns(), Ordering::Relaxed);
        match reserved? {
            Reserved::Probe(slot) => Some(ProbePermit {
                held: Some((endpoint, slot)),
            }),
            Reserved::Freely => Some(ProbePermit::free()),
        }
    }

    /// Round-robin selection with lock-free fast path.
    ///
    /// Fast path: scan atomic state tags — if a Closed endpoint is found,
    /// return it immediately without locking. Only falls back to Mutex
    /// when all endpoints are non-Closed (rare: circuit breaker tripped).
    fn select_round_robin(&self, now_ms: u64) -> Option<(usize, ProbePermit<'_>)> {
        let n = self.endpoints.len();
        let start = self.round_robin.fetch_add(1, Ordering::Relaxed);

        // Lock-free fast path: first Closed endpoint in round-robin order.
        // A closed circuit reserves nothing, so no lock is needed.
        for i in 0..n {
            let idx = (start + i) % n;
            if self.endpoints[idx].atomic_state.load(Ordering::Relaxed) & TAG_MASK == TAG_CLOSED {
                return Some((idx, ProbePermit::free()));
            }
        }

        // Slow path: all non-Closed, so a reservation may transition
        // Open→HalfOpen and take a probe slot. The first that allows one
        // wins, and nothing else was reserved.
        (0..n).find_map(|i| {
            let idx = (start + i) % n;
            Some((idx, self.reserve(idx, now_ms)?))
        })
    }

    /// Latency-based selection with lock-free fast path.
    ///
    /// Fast path: scan atomic latency + state for all endpoints without locking.
    /// Among Closed endpoints, pick the one with lowest latency. This is the
    /// steady-state hot path — zero mutex contention.
    ///
    /// Slow path: no Closed endpoints available. Reserve each non-Closed
    /// endpoint, which may transition Open→HalfOpen. Only entered when
    /// circuit breakers have tripped (error condition, rare).
    fn select_latency_based(&self, now_ms: u64) -> Option<(usize, ProbePermit<'_>)> {
        // Lock-free fast path: best Closed endpoint by latency. A closed
        // circuit reserves nothing, so no lock is needed.
        let mut best: Option<(usize, u64)> = None;
        let mut any_non_closed = false;
        for (i, endpoint) in self.endpoints.iter().enumerate() {
            if endpoint.atomic_state.load(Ordering::Relaxed) & TAG_MASK == TAG_CLOSED {
                let latency = endpoint.atomic_latency_ns.load(Ordering::Relaxed);
                if best.is_none_or(|(_, best_latency)| latency < best_latency) {
                    best = Some((i, latency));
                }
            } else {
                any_non_closed = true;
            }
        }
        if let Some((idx, _)) = best {
            return Some((idx, ProbePermit::free()));
        }
        if !any_non_closed {
            return None;
        }

        // Slow path: nothing is closed, so each candidate must be reserved
        // to be considered — and every candidate this loop passes over
        // gives its slot back when its permit drops. Without that, one
        // selection would strand a probe slot on every endpoint it did not
        // choose, and with the default budget of one slot those endpoints
        // would never be probed again.
        let mut best: Option<(usize, u64, ProbePermit<'_>)> = None;
        for idx in 0..self.endpoints.len() {
            let Some(permit) = self.reserve(idx, now_ms) else {
                continue;
            };
            let latency = self.endpoints[idx]
                .atomic_latency_ns
                .load(Ordering::Relaxed);
            match &best {
                // Keeping the incumbent drops this candidate's permit.
                Some((_, best_latency, _)) if *best_latency <= latency => {}
                // Replacing it drops the incumbent's permit.
                _ => best = Some((idx, latency, permit)),
            }
        }
        best.map(|(idx, _, permit)| (idx, permit))
    }

    /// Select up to `n` callable endpoints for hedged requests, ordered by latency.
    ///
    /// Uses a fixed-size stack buffer (max 16 endpoints) to avoid heap allocation
    /// in the common case.
    pub fn select_n(&self, n: usize, now_ms: u64) -> Vec<(usize, ProbePermit<'_>)> {
        self.select_n_between(n, now_ms, |_| {})
    }

    /// [`Self::select_n`], running `between` after the lock-free scan, where
    /// another request may change an endpoint's circuit.
    fn select_n_between(
        &self,
        n: usize,
        now_ms: u64,
        between: impl FnOnce(&Self),
    ) -> Vec<(usize, ProbePermit<'_>)> {
        let mut candidates: Vec<(usize, u64, ProbePermit<'_>)> = Vec::with_capacity(n);
        let mut any_non_closed = false;

        // Lock-free fast path: collect Closed endpoints, which reserve
        // nothing.
        for (idx, endpoint) in self.endpoints.iter().enumerate() {
            if endpoint.atomic_state.load(Ordering::Relaxed) & TAG_MASK == TAG_CLOSED {
                let latency = endpoint.atomic_latency_ns.load(Ordering::Relaxed);
                candidates.push((idx, latency, ProbePermit::free()));
            } else {
                any_non_closed = true;
            }
        }

        between(self);

        // Only reserve elsewhere if the closed endpoints cannot fill the
        // fan-out: a reservation this function hands back is a probe slot
        // spent on a request that never happens. An endpoint the first scan
        // took is skipped even if its circuit has since opened, so no
        // endpoint is selected twice.
        if candidates.len() < n && any_non_closed {
            let taken = candidates.len();
            for idx in 0..self.endpoints.len() {
                if self.endpoints[idx].atomic_state.load(Ordering::Relaxed) & TAG_MASK == TAG_CLOSED
                    || candidates[..taken].iter().any(|&(held, _, _)| held == idx)
                {
                    continue;
                }
                if let Some(permit) = self.reserve(idx, now_ms) {
                    let latency = self.endpoints[idx]
                        .atomic_latency_ns
                        .load(Ordering::Relaxed);
                    candidates.push((idx, latency, permit));
                }
            }
        }

        // Fastest first; the permits of the candidates that do not make the
        // cut drop with the truncation, giving their slots back.
        candidates.sort_unstable_by_key(|&(_, latency, _)| latency);
        candidates.truncate(n);
        candidates
            .into_iter()
            .map(|(idx, _, permit)| (idx, permit))
            .collect()
    }

    /// Number of endpoints in this pool.
    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    /// Record a successful request on endpoint `idx`, sent under `permit`:
    /// the permit's probe slot closes a half-open circuit, and a success
    /// under a permit that holds none leaves the circuit as it is.
    pub fn record_success(&self, idx: usize, latency_ns: u64, permit: ProbePermit<'_>) {
        let endpoint = &self.endpoints[idx];
        endpoint.record_success(latency_ns, permit.into_slot(endpoint));
    }

    /// Record a failed request on endpoint `idx`.
    pub fn record_failure(&self, idx: usize, now_ms: u64) {
        self.endpoints[idx].record_failure(now_ms);
    }

    /// Record what `response`, an answer to a read ([`answered_read`])
    /// sent under `permit`, says about endpoint `idx`: a success, unless it
    /// holds a rate limit, which is a failure.
    pub(crate) fn record_read(
        &self,
        idx: usize,
        now_ms: u64,
        latency_ns: u64,
        response: &ResponsePacket,
        permit: ProbePermit<'_>,
    ) {
        if !response.iter_errors().any(is_rate_limit) {
            self.record_success(idx, latency_ns, permit);
        } else {
            self.record_failure(idx, now_ms);
        }
    }

    /// Record what a failed request, sent under `permit`, says about
    /// endpoint `idx`.
    ///
    /// A request the endpoint declined is not held against it — it
    /// answered, and every endpoint would decline the same request — but
    /// the answer is still evidence it is alive, which ends its failure
    /// streak and releases the probe slot `permit` holds, and no other.
    /// See [`Fault`](super::fault::Fault).
    ///
    /// An error response out of the transport is never an answer: an
    /// endpoint's error under a request's id comes back inside the
    /// response, so alloy returns one only for a body that is no JSON-RPC
    /// response, such as a bare error object with no id. It answers no
    /// request, so it counts against the endpoint.
    pub fn record_error(
        &self,
        idx: usize,
        now_ms: u64,
        error: &TransportError,
        permit: ProbePermit<'_>,
    ) {
        if error.as_error_resp().is_some() {
            return self.record_failure(idx, now_ms);
        }
        match fault(error) {
            Fault::Endpoint => self.record_failure(idx, now_ms),
            Fault::Request => {
                let endpoint = &self.endpoints[idx];
                endpoint.record_answered(permit.into_slot(endpoint));
            }
        }
    }

    /// Clone the transport for endpoint `idx` (cheap, clones the inner Arc).
    fn transport(&self, idx: usize) -> alloy::transports::BoxTransport {
        self.endpoints[idx].transport.clone()
    }

    /// Host-only label of endpoint `idx`, safe for tracing.
    fn label(&self, idx: usize) -> &str {
        &self.endpoints[idx].label
    }

    /// `err`'s text with every trace of endpoint `idx`'s URL cut, safe for
    /// tracing.
    fn redacted(&self, idx: usize, err: &TransportError) -> String {
        super::redact::redact_in(&err.to_string(), &self.endpoints[idx].url)
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

    /// URLs of all endpoints in this pool, credentials included. Never log
    /// these; use [`redact_url`](super::redact_url) for diagnostics.
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
    /// Set once any endpoint has answered `eth_getProof` with "method not
    /// found"; see [`HftTransport::supports_get_proof`].
    get_proof_unsupported: AtomicBool,
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
                get_proof_unsupported: AtomicBool::new(false),
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

    /// Whether `eth_getProof` is still worth asking for over this
    /// transport: false once any endpoint has answered it with "method not
    /// found" (JSON-RPC `-32601`), after which proof-backed reads go
    /// straight to their `eth_getStorageAt` fallback, which every endpoint
    /// serves.
    ///
    /// One latch for the whole endpoint set, shared by every client over
    /// this transport; a per-endpoint record is #116.
    pub(crate) fn supports_get_proof(&self) -> bool {
        !self.router.get_proof_unsupported.load(Ordering::Relaxed)
    }

    /// Remember that an endpoint answered `eth_getProof` with "method not
    /// found". Relaxed ordering: the latch is a hint, and a lost race costs
    /// one more probe.
    pub(crate) fn note_get_proof_unsupported(&self) {
        self.router
            .get_proof_unsupported
            .store(true, Ordering::Relaxed);
    }
}

// ── JSON-RPC method classification ──────────────────────────────────

/// The methods this transport sends, retries and hedges as reads: reads
/// without side effects, the SDK's own and the common ones next to them.
/// Every other method, a new or unknown one included, is refused unless it
/// is a single `eth_sendRawTransaction` ([`refused`]).
pub(crate) const READ_METHODS: &[&str] = &[
    "eth_blobBaseFee",
    "eth_blockNumber",
    "eth_call",
    "eth_chainId",
    "eth_estimateGas",
    "eth_feeHistory",
    "eth_gasPrice",
    "eth_getBalance",
    "eth_getBlockByHash",
    "eth_getBlockByNumber",
    "eth_getCode",
    "eth_getLogs",
    "eth_getProof",
    "eth_getStorageAt",
    "eth_getTransactionByHash",
    "eth_getTransactionCount",
    "eth_getTransactionReceipt",
    "eth_maxPriorityFeePerGas",
    "eth_syncing",
    "net_version",
    "web3_clientVersion",
];

/// The one write this transport sends: a single signed raw transaction.
/// It is sent again, as the same signed bytes, and its error answer under
/// its own id is the node's rejection.
const RAW_SEND: &str = "eth_sendRawTransaction";

fn is_read(method: &str) -> bool {
    READ_METHODS.contains(&method)
}

/// The error that refuses `req` before it is sent, or `None` when it is
/// sent: a request is sent only when the JSON body of each call holds the
/// method and id of its metadata ([`body_disagrees`]), and it is a single
/// call to a method on [`READ_METHODS`], a batch of such calls, or a
/// single `eth_sendRawTransaction` with an id. A raw send or a read with a
/// null id is refused because alloy reads it the same as an answer with no
/// id, a batch whose calls repeat an id because its answers could not be
/// told apart, and an empty batch because a node answers it with one error
/// and not a batch; every other method, a write or an unknown method, and
/// any batch that holds one, because this transport sends only signed raw
/// transactions.
fn refused(req: &RequestPacket) -> Option<TransportError> {
    let calls = match req {
        RequestPacket::Single(call) => std::slice::from_ref(call),
        RequestPacket::Batch(calls) => calls.as_slice(),
    };
    if calls.is_empty() {
        return Some(TransportError::local_usage_str(
            "an empty batch has no answer under an id: it was not sent",
        ));
    }
    if let Some(error) = calls.iter().find_map(body_disagrees) {
        return Some(error);
    }
    if let Some(call) = calls
        .iter()
        .find(|call| is_read(call.method()) && call.id().is_none())
    {
        return Some(TransportError::local_usage_str(&format!(
            "a {} read with a null id has an answer that could not be told apart: it was not sent",
            call.method()
        )));
    }
    if let Some((at, call)) = calls
        .iter()
        .enumerate()
        .find(|(at, call)| calls[..*at].iter().any(|c| c.id() == call.id()))
    {
        return Some(TransportError::local_usage_str(&format!(
            "the batch repeats id {} at call {at}, so its answers could not be told apart: it \
             was not sent",
            call.id()
        )));
    }
    let (method, batch) = match req {
        RequestPacket::Single(call) if call.method() == RAW_SEND => {
            return call.id().is_none().then(|| {
                TransportError::local_usage_str(
                    "an eth_sendRawTransaction with a null id has an answer that could not be \
                     told apart: it was not sent",
                )
            });
        }
        RequestPacket::Single(call) if is_read(call.method()) => return None,
        RequestPacket::Single(call) => (call.method(), ""),
        RequestPacket::Batch(calls) => (
            calls.iter().find(|call| !is_read(call.method()))?.method(),
            "; a batch may hold only reads",
        ),
    };
    Some(TransportError::local_usage_str(&format!(
        "HftTransport sends only single signed eth_sendRawTransaction writes; {method} is not \
         supported{batch}"
    )))
}

/// The method and id that a call's JSON body holds. A repeated top-level
/// key is an error, so a body cannot name one method, id or params to this
/// check and another to the node.
struct Envelope {
    method: String,
    id: alloy::rpc::json_rpc::Id,
}

impl Envelope {
    /// The envelope of `body`, which must be one JSON object with no
    /// repeated top-level key: a positional array, which serde reads into
    /// a struct as its fields in order, is no call.
    fn parse(body: &str) -> serde_json::Result<Self> {
        struct Object;
        impl<'de> serde::de::Visitor<'de> for Object {
            type Value = Envelope;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON-RPC call object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Envelope, A::Error> {
                let mut keys = HashSet::new();
                let (mut method, mut id) = (None, None);
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "method" => method = Some(map.next_value::<String>()?),
                        "id" => id = Some(map.next_value::<alloy::rpc::json_rpc::Id>()?),
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                    if !keys.insert(key) {
                        return Err(A::Error::custom("a repeated key"));
                    }
                }
                Ok(Envelope {
                    method: method.ok_or_else(|| A::Error::missing_field("method"))?,
                    id: id.ok_or_else(|| A::Error::missing_field("id"))?,
                })
            }
        }
        let mut de = serde_json::Deserializer::from_str(body);
        let envelope = (&mut de).deserialize_map(Object)?;
        de.end()?;
        Ok(envelope)
    }
}

/// The error that refuses `call` when its JSON body, which is what the
/// endpoint gets, does not hold its metadata's method and id (compared as
/// typed), or is no single call: every check here reads the metadata, so
/// a call whose body says otherwise is not sent.
fn body_disagrees(call: &SerializedRequest) -> Option<TransportError> {
    match Envelope::parse(call.serialized().get()) {
        Ok(body) if body.method == call.method() && body.id == *call.id() => None,
        _ => Some(TransportError::local_usage_str(&format!(
            "the JSON body of a {} call with id {} does not hold that method and id: it was not \
             sent",
            call.method(),
            call.id()
        ))),
    }
}

/// The raw send that `req` is, or `None` for a read.
fn raw_send(req: &RequestPacket) -> Option<&SerializedRequest> {
    match req {
        RequestPacket::Single(call) if call.method() == RAW_SEND => Some(call),
        _ => None,
    }
}

/// The transport error that replaces `response` to the raw send `call`
/// when the response is no answer to it, so the outcome of the write is
/// unknown; `None` when it is one.
///
/// A raw send is answered only by one response with its own id, compared
/// as typed (the number `0` is not the string `"0"`), on the error path
/// and the success path alike, and its success only by a transaction
/// hash, so that an error object in the result's place is never mistaken
/// for the node's rejection.
fn unanswered_write(call: &SerializedRequest, response: &ResponsePacket) -> Option<TransportError> {
    let reason = match response {
        ResponsePacket::Single(answer) if answer.id == *call.id() => match &answer.payload {
            ResponsePayload::Success(result)
                if serde_json::from_str::<B256>(result.get()).is_err() =>
            {
                "the write was answered with a result that is no transaction hash".to_owned()
            }
            _ => return None,
        },
        _ => format!("the write was not answered under its id {}", call.id()),
    };
    Some(TransportErrorKind::custom_str(&format!(
        "{reason}: the outcome of the write is unknown"
    )))
}

/// The read `req`'s result, with a response that is no answer to it made
/// a transport error, so it is an endpoint failure and the read is asked
/// again ([`answered_read`]). An error answer under the call's own id
/// stays an answer.
fn read_result(
    req: &RequestPacket,
    result: Result<ResponsePacket, TransportError>,
) -> Result<ResponsePacket, TransportError> {
    match result {
        Ok(response) if !answered_read(req, &response) => Err(TransportErrorKind::custom_str(
            "the read was not answered under its id",
        )),
        result => result,
    }
}

/// Whether `response` answers the read `req`: each call by its id,
/// compared as typed (a batch by a batch with one answer for each call,
/// as many under an id as calls carry it). An answer under another id or
/// of another shape answers nothing, and alloy, which takes a single
/// answer without reading its id, would hand it to the caller.
fn answered_read(req: &RequestPacket, response: &ResponsePacket) -> bool {
    match (req, response) {
        (RequestPacket::Single(call), ResponsePacket::Single(answer)) => answer.id == *call.id(),
        (RequestPacket::Batch(calls), ResponsePacket::Batch(answers)) => {
            answers.len() == calls.len()
                && calls.iter().all(|call| {
                    let carried = calls.iter().filter(|c| c.id() == call.id()).count();
                    answers.iter().filter(|a| a.id == *call.id()).count() == carried
                })
        }
        _ => false,
    }
}

/// The transport error that replaces `error`, the transport's failure on
/// a raw send. A body that does not read as a JSON-RPC response is no
/// answer, even when alloy reads it as an error object (an `ErrorResp`
/// with no id): only an error response under the write's own id
/// ([`unanswered_write`]) is the node's rejection.
fn undecoded_write(error: TransportError) -> TransportError {
    match error.as_error_resp() {
        Some(body) => TransportErrorKind::custom_str(&format!(
            "the write's response is no JSON-RPC response, and its body reads {} {:?}: the \
             outcome of the write is unknown",
            body.code, body.message
        )),
        None => error,
    }
}

/// The error that replaces the last answer to a single
/// `eth_sendRawTransaction` when that answer is an explicit rejection
/// ([`is_rejection`]) after an earlier attempt that may have left the
/// transaction with a node (`may_be_held`). A transport error, so no
/// caller reads it as proof that no node holds the transaction.
fn doubtful_rejection(response: &ResponsePacket, may_be_held: bool) -> Option<TransportError> {
    if !may_be_held {
        return None;
    }
    let error = response.iter_errors().find(|error| is_rejection(error))?;
    Some(TransportErrorKind::custom_str(&format!(
        "the write was answered {} {:?} after an earlier attempt that may have left a \
         transaction with a node",
        error.code, error.message
    )))
}

// ── Pool-aware routing ─────────────────────────────────────────────

impl Router {
    /// Select the pool and endpoint index for a request.
    ///
    /// Tries the dedicated pool first (read or write), then falls back to
    /// the shared pool. Returns a reference to the chosen pool and the
    /// endpoint index within that pool, or `None` if all endpoints across
    /// both pools are unavailable.
    fn select_for(
        &self,
        is_write: bool,
        now_ms: u64,
    ) -> Option<(&EndpointPool, usize, ProbePermit<'_>)> {
        let dedicated = if is_write { &self.write } else { &self.read };

        // Try dedicated pool first
        if !dedicated.is_empty()
            && let Some((idx, permit)) = dedicated.select(self.strategy, now_ms)
        {
            return Some((dedicated, idx, permit));
        }

        // Fall back to shared pool
        self.shared
            .select(self.strategy, now_ms)
            .map(|(idx, permit)| (&self.shared, idx, permit))
    }

    /// Select up to `fan_out` endpoints for a hedged read, with their
    /// permits: from the read pool when it gives any, a half-open endpoint
    /// whose recovery timeout has passed included, else from the shared
    /// pool. Asking the read pool's breakers, not counting its closed
    /// circuits, is what lets a read endpoint that recovers serve again.
    fn select_hedged(
        &self,
        fan_out: usize,
        now_ms: u64,
    ) -> (&EndpointPool, Vec<(usize, ProbePermit<'_>)>) {
        let selected = self.read.select_n(fan_out, now_ms);
        if !selected.is_empty() {
            return (&self.read, selected);
        }
        (&self.shared, self.shared.select_n(fan_out, now_ms))
    }

    /// Route a request through the best endpoint, with retry.
    ///
    /// A request that is neither a read on [`READ_METHODS`] (or a batch of
    /// them) nor a single `eth_sendRawTransaction` with an id is refused
    /// before it is sent ([`refused`]). A read retries, on any transport or
    /// RPC error that is not the endpoint declining it, a response that
    /// answers no call by its id ([`read_result`]) and a body that is no
    /// JSON-RPC response included, and is hedged; an answer counts against
    /// the endpoint's health when it holds a rate limit. A raw send
    /// retries on an error answer (e.g. `-32003 insufficient funds` from a
    /// stale read replica) and on no answer, always as the same signed
    /// bytes; its last explicit rejection ([`is_rejection`]) is returned as
    /// the node gave it only when no earlier attempt may have left the
    /// transaction with a node, and comes back as a transport error
    /// otherwise. A raw send's response that is no answer to it
    /// ([`unanswered_write`]: another id, another shape, a success that is
    /// no transaction hash) is a transport error and an endpoint failure,
    /// the write's outcome unknown, and so is a body that is no JSON-RPC
    /// response ([`undecoded_write`]).
    async fn route_request(
        self: &Arc<Self>,
        req: RequestPacket,
    ) -> Result<ResponsePacket, TransportError> {
        if let Some(error) = refused(&req) {
            return Err(error);
        }
        let raw_send = raw_send(&req);
        let is_write = raw_send.is_some();
        let retry = if is_write {
            (
                self.config.write_retry.max_retries,
                self.config.write_retry.base_delay,
            )
        } else {
            (
                self.config.read_retry.max_retries,
                self.config.read_retry.base_delay,
            )
        };
        let (max_attempts, base_delay) = (1 + retry.0, retry.1);
        let timeout = self.config.request_timeout;

        // Handle hedged reads
        if !is_write && let Strategy::Hedged { fan_out } = self.strategy {
            return self.hedged_request(req, fan_out, timeout).await;
        }

        // Standard path: select endpoint from appropriate pool, try with retry
        let mut last_err = None;
        let mut may_be_held = false;

        for attempt in 0..max_attempts {
            // An outcome is stamped with the time it arrived, not the time
            // its request left: a circuit a slow failure opens would
            // otherwise start its recovery window early.
            let Some((pool, idx, permit)) = self.select_for(is_write, now_ms()) else {
                tracing::error!("all RPC endpoints unavailable (circuits open)");
                return Err(TransportError::local_usage_str(
                    "all RPC endpoints unavailable (circuits open)",
                ));
            };

            let start = Instant::now();
            let mut transport = pool.transport(idx);

            // Apply tower timeout
            let result = match (
                tokio::time::timeout(timeout, transport.call(req.clone())).await,
                raw_send,
            ) {
                (Ok(Ok(response)), Some(call)) => match unanswered_write(call, &response) {
                    Some(error) => Ok(Err(error)),
                    None => Ok(Ok(response)),
                },
                (Ok(Err(error)), Some(_)) => Ok(Err(undecoded_write(error))),
                (Ok(result), None) => Ok(read_result(&req, result)),
                (result, _) => result,
            };

            match result {
                Ok(Ok(response)) => {
                    // A raw send's error answer is retried as the same
                    // signed bytes.
                    if is_write && self.config.write_retry.is_retriable(&response) {
                        let rejected = response.iter_errors().any(is_rejection);
                        // Stale-replica rejections are not evidence of an
                        // unhealthy endpoint — don't touch the circuit breaker.
                        if attempt + 1 < max_attempts {
                            may_be_held |= !rejected;
                            // This attempt is over and the next one selects
                            // again, so give any probe slot back now rather
                            // than hold it through the backoff, where it
                            // would keep the endpoint from another caller.
                            drop(permit);
                            tracing::warn!(
                                attempt = attempt + 1,
                                max_attempts,
                                endpoint = %pool.label(idx),
                                error_code = response.first_error_code(),
                                "write answered with an error, retrying"
                            );
                        } else {
                            tracing::warn!(
                                endpoint = %pool.label(idx),
                                error_code = response.first_error_code(),
                                "write answered with an error on its last attempt"
                            );
                            if let Some(error) = doubtful_rejection(&response, may_be_held) {
                                return Err(error);
                            }
                            return Ok(response);
                        }
                    } else {
                        let latency_ns = start.elapsed().as_nanos() as u64;
                        if is_write {
                            pool.record_success(idx, latency_ns, permit);
                        } else {
                            pool.record_read(idx, now_ms(), latency_ns, &response, permit);
                        }
                        return Ok(response);
                    }
                }
                Ok(Err(e)) => {
                    pool.record_error(idx, now_ms(), &e, permit);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        endpoint = %pool.label(idx),
                        error = %pool.redacted(idx, &e),
                        is_write,
                        "transport error"
                    );
                    // A declined read earns the same answer however often
                    // it is asked, so it goes back to the caller now:
                    // retrying costs round trips and delays the reply it
                    // is going to get anyway. An error response out of the
                    // transport declined nothing: it is a body that is no
                    // JSON-RPC response ([`EndpointPool::record_error`]).
                    // Writes keep their own retry policy, which turns on
                    // whether the transaction could have landed.
                    if !is_write && fault(&e) == Fault::Request && e.as_error_resp().is_none() {
                        return Err(e);
                    }
                    may_be_held = true;
                    last_err = Some(e);
                }
                Err(_timeout) => {
                    // The failure opened a half-open circuit, so the
                    // permit's slot has nothing left to give back.
                    pool.record_failure(idx, now_ms());
                    drop(permit);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        endpoint = %pool.label(idx),
                        is_write,
                        "request timed out"
                    );
                    may_be_held = true;
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

    /// Fan out a read request to the endpoints [`Self::select_hedged`]
    /// gives, return the fastest answer ([`read_result`]): a response under
    /// another id is an endpoint failure, and the other requests run on.
    ///
    /// Uses [`JoinSet`] to properly cancel losing requests via `abort_all()`,
    /// saving RPC rate limits and network bandwidth. Health is recorded for all
    /// endpoints that complete before cancellation.
    async fn hedged_request(
        &self,
        req: RequestPacket,
        fan_out: usize,
        timeout: std::time::Duration,
    ) -> Result<ResponsePacket, TransportError> {
        let (pool, selected) = self.select_hedged(fan_out, now_ms());

        if selected.is_empty() {
            return Err(TransportError::local_usage_str(
                "all RPC endpoints unavailable (circuits open)",
            ));
        }

        // Permits stay here, in the parent: a losing request is cancelled
        // and never reports an outcome, so only dropping its permit gives
        // the probe slot back. Keyed by endpoint so an outcome can resolve
        // the right one.
        let mut permits: Vec<(usize, ProbePermit<'_>)> = selected;

        // If only one endpoint is available, fall back to single request
        if permits.len() == 1 {
            let (idx, permit) = permits.pop().expect("one endpoint");
            let start = Instant::now();
            let mut transport = pool.transport(idx);
            let result = tokio::time::timeout(timeout, transport.call(req.clone()))
                .await
                .map(|result| read_result(&req, result));

            return match result {
                Ok(Ok(resp)) => {
                    let latency_ns = start.elapsed().as_nanos() as u64;
                    pool.record_read(idx, now_ms(), latency_ns, &resp, permit);
                    Ok(resp)
                }
                Ok(Err(e)) => {
                    pool.record_error(idx, now_ms(), &e, permit);
                    Err(e)
                }
                Err(_) => {
                    pool.record_failure(idx, now_ms());
                    drop(permit);
                    Err(TransportError::local_usage_str("request timed out"))
                }
            };
        }

        // Fan out to multiple endpoints using JoinSet for proper cancellation
        let mut join_set = tokio::task::JoinSet::new();

        for &(idx, _) in &permits {
            let mut transport = pool.transport(idx);
            let req_clone = req.clone();

            join_set.spawn(async move {
                let start = Instant::now();
                let result = tokio::time::timeout(timeout, transport.call(req_clone.clone())).await;
                let result = match result {
                    Ok(r) => read_result(&req_clone, r),
                    Err(_) => Err(TransportError::local_usage_str("request timed out")),
                };
                (idx, result, start)
            });
        }

        /// Take out the permit of the endpoint that just reported, so its
        /// outcome — not its drop — settles the probe slot.
        fn take<'p>(permits: &mut Vec<(usize, ProbePermit<'p>)>, idx: usize) -> ProbePermit<'p> {
            permits
                .iter()
                .position(|&(held, _)| held == idx)
                .map_or_else(ProbePermit::free, |at| permits.swap_remove(at).1)
        }

        let mut last_err = None;

        while let Some(join_result) = join_set.join_next().await {
            match join_result {
                Ok((idx, Ok(response), start)) => {
                    let latency_ns = start.elapsed().as_nanos() as u64;
                    let permit = take(&mut permits, idx);
                    pool.record_read(idx, now_ms(), latency_ns, &response, permit);
                    // Cancel remaining in-flight requests — saves RPC rate limits.
                    // JoinSet::drop also aborts, but explicit abort_all is clearer.
                    // The cancelled requests' permits drop with `permits`.
                    join_set.abort_all();
                    return Ok(response);
                }
                Ok((idx, Err(e), _start)) => {
                    let permit = take(&mut permits, idx);
                    pool.record_error(idx, now_ms(), &e, permit);
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
    use alloy::eips::BlockNumberOrTag;
    use alloy::primitives::{Address, Bytes, U256};
    #[allow(unused_imports)]
    use alloy::providers::Provider as _;
    use alloy::rpc::json_rpc::Id;
    use alloy::rpc::types::{Filter, TransactionRequest};
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Notify;

    use super::*;
    use crate::transport::config::{Strategy, TransportConfig, WriteRetryConfig};
    use crate::transport::scripted::{self, Answer};

    /// Helper to create a SerializedRequest for testing.
    fn make_request(method: &'static str, id: u64) -> SerializedRequest {
        make_request_with(method, alloy::rpc::json_rpc::Id::Number(id))
    }

    fn make_request_with(method: &'static str, id: alloy::rpc::json_rpc::Id) -> SerializedRequest {
        let params = serde_json::value::RawValue::from_string("[]".to_string()).unwrap();
        alloy::rpc::json_rpc::Request::new(method, id, params)
            .serialize()
            .unwrap()
    }

    /// Every method a trading transport never sends: node-signed and
    /// synchronous sends, conditional sends, signing, subscriptions over
    /// HTTP, an unknown method and an empty name.
    const REFUSED: [&str; 10] = [
        "eth_sendTransaction",
        "eth_sendTransactionSync",
        "eth_sendRawTransactionSync",
        "eth_sendRawTransactionConditional",
        "personal_sendTransaction",
        "personal_sign",
        "eth_sign",
        "eth_subscribe",
        "eth_futureMethod",
        "",
    ];

    /// The local error that refuses `method`, naming it, and for a batch
    /// saying that a batch may hold only reads.
    fn assert_refused(error: &TransportError, method: &str, in_batch: bool) {
        assert!(
            matches!(error, alloy::transports::RpcError::LocalUsageError(_)),
            "{method}: {error:?}"
        );
        let expected = format!(
            "HftTransport sends only single signed eth_sendRawTransaction writes; {method} is not \
             supported"
        );
        assert!(error.to_string().contains(&expected), "{method}: {error}");
        assert_eq!(
            error.to_string().contains("; a batch may hold only reads"),
            in_batch,
            "{method}: {error}"
        );
    }

    /// Only a read, a batch of reads and a single raw send with an id are
    /// sent; every other method, and any batch that holds one (a raw send
    /// included), is refused with a local error that names it.
    #[test]
    fn refuses_every_write_but_a_single_raw_send() {
        for method in READ_METHODS {
            let read = RequestPacket::Single(make_request(method, 1));
            assert!(refused(&read).is_none(), "{method}");
        }
        assert!(refused(&RequestPacket::Single(make_request(RAW_SEND, 1))).is_none());
        let reads = RequestPacket::Batch(vec![
            make_request("eth_getBalance", 1),
            make_request("eth_chainId", 2),
        ]);
        assert!(refused(&reads).is_none());

        for method in REFUSED.into_iter().chain([RAW_SEND]) {
            if method != RAW_SEND {
                let single = RequestPacket::Single(make_request(method, 1));
                assert_refused(&refused(&single).expect("refused"), method, false);
            }
            let mixed = RequestPacket::Batch(vec![
                make_request("eth_getBalance", 1),
                make_request(method, 2),
            ]);
            assert_refused(&refused(&mixed).expect("refused"), method, true);
            let alone = RequestPacket::Batch(vec![make_request(method, 1)]);
            assert_refused(&refused(&alone).expect("refused"), method, true);
        }
    }

    #[test]
    fn new_transport_shared_only() {
        let config = TransportConfig::builder()
            .shared_endpoint("https://arb1.arbitrum.io/rpc")
            .shared_endpoint("https://arbitrum-one-rpc.publicnode.com")
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
        let a = pool.select(Strategy::RoundRobin, now).unwrap().0;
        let b = pool.select(Strategy::RoundRobin, now).unwrap().0;
        let c = pool.select(Strategy::RoundRobin, now).unwrap().0;
        let d = pool.select(Strategy::RoundRobin, now).unwrap().0;

        // Should cycle through 0, 1, 2, 0
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_eq!(c, 2);
        assert_eq!(d, 0);
    }

    /// Selecting among several recovering endpoints reserves a probe slot
    /// on each one it considers, and with the default budget of one slot a
    /// reservation never given back would remove that endpoint from the
    /// pool for good. The permits the selection passes over release theirs,
    /// so every endpoint stays reachable.
    #[test]
    fn selection_does_not_strand_probe_slots_on_endpoints_it_passes_over() {
        let pool = EndpointPool::from_urls(
            &[
                "https://rpc1.example.com".into(),
                "https://rpc2.example.com".into(),
                "https://rpc3.example.com".into(),
            ],
            Default::default(),
        )
        .unwrap();

        // Trip every circuit, then let the recovery window elapse.
        for idx in 0..3 {
            for t in 1..=3 {
                pool.record_failure(idx, t * 1_000);
            }
        }
        let recovered = 40_000;
        assert!(pool.select(Strategy::LatencyBased, recovered).is_some());

        // Whichever endpoint was chosen, the other two were reserved while
        // being considered. If their slots were stranded, every later
        // selection would find nothing callable.
        for round in 0..5 {
            assert!(
                pool.select(Strategy::LatencyBased, recovered).is_some(),
                "no endpoint callable on round {round}: probe slots were stranded"
            );
        }

        // Same for the hedged selection, which considers every endpoint and
        // keeps only the fastest `n`.
        assert_eq!(pool.select_n(1, recovered).len(), 1);
        for round in 0..5 {
            assert_eq!(
                pool.select_n(1, recovered).len(),
                1,
                "hedged selection stranded a slot on round {round}"
            );
        }
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

        pool.record_success(0, 10_000_000, ProbePermit::free()); // 10ms
        pool.record_success(1, 1_000_000, ProbePermit::free()); // 1ms

        let (selected, _permit) = pool.select(Strategy::LatencyBased, now_ms()).unwrap();
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

        let (selected, _permit) = pool.select(Strategy::LatencyBased, now).unwrap();
        assert_eq!(selected, 1); // only healthy endpoint
    }

    /// A provider that declines a request — a block range it considers
    /// too wide, say — is working, so it stays in service however many
    /// times it declines. The log scan's width search depends on this:
    /// narrowing from its initial span takes several rejections in a row,
    /// and a breaker that counted them would take the endpoint out before
    /// the search converged.
    #[test]
    fn declined_requests_leave_an_endpoint_in_service() {
        let pool =
            EndpointPool::from_urls(&["https://rpc1.example.com".into()], Default::default())
                .unwrap();
        let now = now_ms();

        let declined = TransportErrorKind::http_error(400, String::new());
        for _ in 0..10 {
            pool.record_error(0, now, &declined, ProbePermit::free());
        }
        assert_eq!(
            pool.select(Strategy::LatencyBased, now).map(|(idx, _)| idx),
            Some(0),
            "an endpoint that declines requests is still working"
        );

        // An endpoint that cannot answer still loses its circuit.
        let overloaded = TransportErrorKind::http_error(503, String::new());
        for _ in 0..3 {
            pool.record_error(0, now, &overloaded, ProbePermit::free());
        }
        assert!(pool.select(Strategy::LatencyBased, now).is_none());
    }

    /// Alchemy's reply to an `eth_getLogs` range it considers too wide: a
    /// JSON-RPC error delivered with an HTTP client-error status.
    fn declined_response() -> String {
        const BODY: &str = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"Log response size exceeded. Retry a smaller range."}}"#;
        format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{BODY}",
            BODY.len(),
        )
    }

    /// A local endpoint that declines everything, counting what it was
    /// asked. Answers on the same connection as often as it is asked, so
    /// the count is requests, not connections.
    async fn declining_endpoint() -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let served = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&served);
        tokio::spawn(async move {
            let response = declined_response();
            while let Ok((mut socket, _)) = listener.accept().await {
                let counter = Arc::clone(&counter);
                let response = response.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    while let Ok(read) = socket.read(&mut buf).await {
                        if read == 0 || socket.write_all(response.as_bytes()).await.is_err() {
                            return;
                        }
                        counter.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });
        (url, served)
    }

    fn provider_over(url: &str) -> alloy::providers::RootProvider<alloy::network::Ethereum> {
        let transport = HftTransport::new(
            TransportConfig::builder()
                .shared_endpoint(url)
                .build()
                .unwrap(),
        )
        .unwrap();
        alloy::providers::RootProvider::new(alloy::rpc::client::RpcClient::new(
            alloy::transports::BoxTransport::new(transport),
            false,
        ))
    }

    /// A write that every attempt answered with an error: an explicit
    /// rejection is passed on as the node's answer only when every
    /// attempt got one; after an attempt with no answer or with another
    /// answer, the node can hold the transaction, so the last rejection
    /// comes back as a transport error that names it. Two, three and four
    /// attempts, the doubtful one anywhere but last.
    #[tokio::test]
    async fn a_write_rejection_is_the_nodes_answer_only_when_every_attempt_got_one() {
        const U: &str = "replacement transaction underpriced";
        let scripts: [(&[&str], bool); 22] = [
            (&[U, U], true),
            (&["already known", U], false),
            (&["nonce too low", U], false),
            (&["HTTP 503", U], false),
            (&["no answer", U], false),
            (&["text id", U], false),
            (&[U, "text id", U], false),
            (&[U, U, U], true),
            (&["no answer", U, U], false),
            (&[U, "no answer", U], false),
            (&[U, "HTTP 503", U], false),
            (&["HTTP 503", U, U], false),
            (&[U, "already known", U], false),
            (&["nonce too low", U, U], false),
            (&["insufficient funds", U, U], true),
            (&[U, U, U, U], true),
            (&[U, U, "no answer", U], false),
            (&["already known", U, U, U], false),
            (
                &[U, "insufficient funds", "Transaction Underpriced", U],
                true,
            ),
            (&["no answer", "bare error"], false),
            (&["bare error", "bare error"], false),
            (&[U, "bare error"], false),
        ];
        for (script, passed_on) in scripts {
            let attempts = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&attempts);
            let url = scripted::endpoint(move |method, _| {
                assert_eq!(method, "eth_sendRawTransaction");
                match script[counter.fetch_add(1, Ordering::SeqCst)] {
                    "no answer" => Answer::Silence,
                    "HTTP 503" => Answer::Status(503),
                    "text id" => Answer::ResultUnderTextId(json!(B256::repeat_byte(0xbb))),
                    "bare error" => Answer::Body(json!({ "code": -32_000, "message": U })),
                    answer => Answer::Error(-32_000, answer.into()),
                }
            })
            .await;
            let retries = u32::try_from(script.len() - 1).unwrap();
            let error = write_provider(&url, retries)
                .send_raw_transaction(&[0x02, 0x01])
                .await
                .expect_err("the last attempt is rejected");
            assert_eq!(attempts.load(Ordering::SeqCst), script.len(), "{script:?}");
            assert_eq!(
                error.as_error_resp().is_some_and(is_rejection),
                passed_on,
                "{script:?}: {error}"
            );
            assert!(error.to_string().contains(U), "{script:?}: {error}");
        }
    }

    /// The answer to a raw send counts only under its own id, compared as
    /// typed, on the error path and the success path alike, and a success
    /// only as a transaction hash.
    #[test]
    fn a_write_is_answered_only_under_its_own_id() {
        const U: &str = r#"{"code":-32000,"message":"replacement transaction underpriced"}"#;
        let hash = format!(r#""{}""#, B256::repeat_byte(0xbb));
        let ok_with =
            |id: &str, result: &str| format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{result}}}"#);
        let ok = |id: &str| ok_with(id, &hash);
        let err = |id: &str| format!(r#"{{"jsonrpc":"2.0","id":{id},"error":{U}}}"#);
        let raw = make_request_with(RAW_SEND, Id::Number(0));
        let raw_text = make_request_with(RAW_SEND, Id::String("0".into()));
        let rows = [
            (&raw, ok("0"), true),
            (&raw, ok(r#""0""#), false),
            (&raw_text, ok(r#""0""#), true),
            (&raw_text, ok("0"), false),
            (&raw, err("0"), true),
            (&raw, err(r#""0""#), false),
            (&raw, err("99"), false),
            (&raw, err("null"), false),
            (&raw, ok("null"), false),
            (&raw, ok("1"), false),
            (&raw, format!("[{}]", ok("0")), false),
            (&raw, ok_with("0", U), false),
            (&raw, ok_with("0", r#""0x01""#), false),
            (&raw, ok_with("0", "null"), false),
        ];
        for (call, body, answered) in rows {
            let response: ResponsePacket = serde_json::from_str(&body).unwrap();
            let unknown = unanswered_write(call, &response);
            assert_eq!(unknown.is_none(), answered, "{body}");
            if let Some(error) = unknown {
                assert!(error.as_error_resp().is_none(), "{body}: {error}");
                assert!(
                    error
                        .to_string()
                        .contains("outcome of the write is unknown"),
                    "{body}: {error}"
                );
            }
        }
    }

    /// A refused request never reaches an endpoint: each method off the
    /// read list but a single raw send, as a single call and in a batch
    /// with a read, a batch of one raw send and a raw send in a batch with
    /// a read, all sent through alloy, fail with the local error, and so do
    /// `send_transaction` and `send_transaction_sync`, which the node would
    /// sign. The scripted endpoint receives no request.
    #[tokio::test]
    async fn a_refused_request_is_not_sent() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let url = scripted::endpoint(move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            Answer::Result(json!(B256::repeat_byte(0xbb)))
        })
        .await;
        let client = write_client(&url, 3);
        for method in REFUSED {
            let error = client
                .request::<_, Value>(method, (json!({}),))
                .await
                .expect_err("refused");
            assert_refused(&error, method, false);

            let mut batch = client.new_batch();
            let read = batch.add_call::<_, Value>("eth_chainId", &()).unwrap();
            let write = batch.add_call::<_, Value>(method, &(json!({}),)).unwrap();
            let error = batch.send().await.expect_err("refused");
            assert_refused(&error, method, true);
            assert!(read.await.is_err() && write.await.is_err(), "{method}");
        }
        for with_read in [false, true] {
            let mut batch = client.new_batch();
            let read = with_read.then(|| batch.add_call::<_, Value>("eth_chainId", &()).unwrap());
            let write = batch
                .add_call::<_, B256>(RAW_SEND, &(Bytes::from_static(&[2, 1]),))
                .unwrap();
            let error = batch.send().await.expect_err("refused");
            assert_refused(&error, RAW_SEND, true);
            assert!(write.await.is_err());
            if let Some(read) = read {
                assert!(read.await.is_err());
            }
        }
        let provider = write_provider(&url, 3);
        let tx = alloy::rpc::types::TransactionRequest::default()
            .from(Address::repeat_byte(0x11))
            .to(Address::repeat_byte(0x22));
        let error = provider
            .send_transaction(tx.clone())
            .await
            .expect_err("refused");
        assert_refused(&error, "eth_sendTransaction", false);
        let error = provider
            .send_transaction_sync(tx)
            .await
            .expect_err("refused");
        assert_refused(&error, "eth_sendTransactionSync", false);
        assert_eq!(requests.load(Ordering::SeqCst), 0);
    }

    /// A raw send or a read with a null id, which alloy reads the same as
    /// an answer with no id, is refused before it is sent.
    #[tokio::test]
    async fn a_call_with_a_null_id_is_not_sent() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let url = scripted::endpoint(move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            Answer::Result(json!(B256::repeat_byte(0xbb)))
        })
        .await;
        let transport = health_transport(&url);
        for method in [RAW_SEND, "eth_chainId"] {
            let error = transport
                .router
                .route_request(RequestPacket::Single(make_request_with(method, Id::None)))
                .await
                .expect_err("refused");
            assert!(
                error.to_string().contains("it was not sent"),
                "{method}: {error}"
            );
        }
        assert_eq!(requests.load(Ordering::SeqCst), 0);
    }

    /// A transport over one endpoint that sends each request once.
    fn health_transport(url: &str) -> HftTransport {
        HftTransport::new(
            TransportConfig::builder()
                .shared_endpoint(url)
                .request_timeout(std::time::Duration::from_millis(300))
                .read_retry(crate::transport::config::ReadRetryConfig {
                    max_retries: 0,
                    base_delay: std::time::Duration::from_millis(1),
                })
                .write_retry(WriteRetryConfig {
                    max_retries: 0,
                    base_delay: std::time::Duration::from_millis(1),
                })
                .build()
                .unwrap(),
        )
        .unwrap()
    }

    /// An answer that does not answer the request counts against the
    /// endpoint, for a read and a raw send alike: an endpoint that answers
    /// every request with an error under id 99, or with a bare error object
    /// with no id (which alloy decodes as an error response), is out of
    /// service after the breaker's threshold of three, where the answer
    /// used to end its failure streak. An error under the request's own id
    /// is the endpoint declining it and leaves it in service.
    #[tokio::test]
    async fn an_answer_under_another_id_is_an_endpoint_failure() {
        const U: &str = "replacement transaction underpriced";
        for (method, id, healthy) in [
            ("eth_chainId", Some(99), false),
            (RAW_SEND, Some(99), false),
            ("eth_chainId", None, false),
            (RAW_SEND, None, false),
            ("eth_chainId", Some(1), true),
        ] {
            let requests = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&requests);
            let error = json!({ "code": -32_000, "message": U });
            let body = match id {
                Some(id) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
                None => error,
            };
            let url = scripted::endpoint(move |_, _| {
                counter.fetch_add(1, Ordering::SeqCst);
                Answer::Body(body.clone())
            })
            .await;
            let transport = health_transport(&url);
            for _ in 0..3 {
                let req = RequestPacket::Single(make_request(method, 1));
                let _ = transport.router.route_request(req).await;
            }
            let case = format!("{method} answered under id {id:?}");
            assert_eq!(requests.load(Ordering::SeqCst), 3, "{case}");
            assert_eq!(transport.healthy_count(), usize::from(healthy), "{case}");
        }
    }

    /// A read that gets no answer under its id is asked again, up to the
    /// read retries, and the caller gets only the answer under its id:
    /// another id or a text id and then the answer returns the answer, a
    /// bare error object with no id and then the answer too, and two
    /// answers under another id are a transport error, never the other
    /// call's result. An error under the read's own id is its answer, sent
    /// once.
    #[tokio::test]
    async fn a_read_answered_under_another_id_is_asked_again() {
        let other = json!({ "jsonrpc": "2.0", "id": 99, "result": "0xdead" });
        let bare = json!({ "code": -32_000, "message": "no" });
        for (script, sent, answered) in [
            (["other id", "ok"], 2, Some("0x1")),
            (["text id", "ok"], 2, Some("0x1")),
            (["bare error", "ok"], 2, Some("0x1")),
            (["other id", "other id"], 2, None),
            (["error", "ok"], 1, Some("error")),
        ] {
            let requests = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&requests);
            let (other, bare) = (other.clone(), bare.clone());
            let url = scripted::endpoint(move |_, _| {
                match script[counter.fetch_add(1, Ordering::SeqCst)] {
                    "other id" => Answer::Body(other.clone()),
                    "text id" => Answer::ResultUnderTextId(json!("0xdead")),
                    "bare error" => Answer::Body(bare.clone()),
                    "error" => Answer::Error(-32_000, "execution reverted".into()),
                    _ => Answer::Result(json!("0x1")),
                }
            })
            .await;
            let transport = HftTransport::new(
                TransportConfig::builder()
                    .shared_endpoint(url)
                    .request_timeout(std::time::Duration::from_millis(300))
                    .read_retry(crate::transport::config::ReadRetryConfig {
                        max_retries: 1,
                        base_delay: std::time::Duration::from_millis(1),
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
            let req = RequestPacket::Single(make_request("eth_blockNumber", 1));
            let result = transport.router.route_request(req).await;
            assert_eq!(requests.load(Ordering::SeqCst), sent, "{script:?}");
            match (result, answered) {
                (Ok(response), Some(expected)) => {
                    let answer = response.as_single().expect("a single answer");
                    assert_eq!(answer.id, alloy::rpc::json_rpc::Id::Number(1), "{script:?}");
                    match &answer.payload {
                        ResponsePayload::Success(result) => {
                            assert_eq!(result.get(), format!("\"{expected}\""), "{script:?}")
                        }
                        ResponsePayload::Failure(_) => assert_eq!(expected, "error", "{script:?}"),
                    }
                }
                (Err(error), None) => {
                    assert!(error.as_error_resp().is_none(), "{script:?}: {error}");
                    assert!(
                        error.to_string().contains("not answered under its id"),
                        "{script:?}: {error}"
                    );
                }
                (result, _) => panic!("{script:?}: {result:?}"),
            }
        }
    }

    /// A hedged read takes only an answer under its id: one endpoint
    /// answers at once under another id, the other 200 ms later under the
    /// read's id, and the caller gets the later answer; the first endpoint
    /// is charged a failure and the second a success.
    #[tokio::test]
    async fn a_hedged_read_waits_for_an_answer_under_its_id() {
        let fast = scripted::endpoint(|_, _| {
            Answer::Body(json!({ "jsonrpc": "2.0", "id": 99, "result": "0xdead" }))
        })
        .await;
        let slow =
            scripted::endpoint(|_, _| Answer::Late(Box::new(Answer::Result(json!("0x2"))))).await;
        let transport = HftTransport::new(
            TransportConfig::builder()
                .strategy(Strategy::Hedged { fan_out: 2 })
                .request_timeout(std::time::Duration::from_secs(2))
                .shared_endpoint(fast)
                .shared_endpoint(slow)
                .build()
                .unwrap(),
        )
        .unwrap();
        let req = RequestPacket::Single(make_request("eth_blockNumber", 7));
        let response = transport
            .router
            .route_request(req)
            .await
            .expect("an answer");
        let answer = response.as_single().expect("a single answer");
        assert_eq!(answer.id, alloy::rpc::json_rpc::Id::Number(7));
        assert_eq!(
            answer.payload.as_success().map(|r| r.get()),
            Some(r#""0x2""#)
        );
        let status = transport.health_status();
        assert_eq!(status[0].consecutive_failures, 1, "{status:?}");
        assert_eq!(status[1].consecutive_failures, 0, "{status:?}");
    }

    /// The write gate reads a call's metadata, but the endpoint gets its
    /// serialized body, so a packet whose two disagree is refused at the
    /// public entry point with a local error and nothing sent: a read's
    /// metadata over a write's body, a raw send's metadata over another
    /// method's body, another id, the same id as text, and one such call
    /// in a batch of reads. A packet whose body agrees is sent.
    #[tokio::test]
    async fn a_call_whose_body_disagrees_with_its_metadata_is_not_sent() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let url = scripted::endpoint(move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            Answer::Result(json!(B256::repeat_byte(0xbb)))
        })
        .await;
        let mut transport = health_transport(&url);
        let disguised = |body: &'static str, method: &'static str, id: Id| {
            let mut call = make_request(body, 1);
            call.meta_mut().method = method.into();
            call.meta_mut().id = id;
            call
        };
        let packets = [
            RequestPacket::Single(disguised("eth_sendTransaction", "eth_call", Id::Number(1))),
            RequestPacket::Single(disguised("eth_sendTransaction", RAW_SEND, Id::Number(1))),
            RequestPacket::Single(disguised("eth_call", "eth_call", Id::Number(2))),
            RequestPacket::Single(disguised("eth_call", "eth_call", Id::String("1".into()))),
            RequestPacket::Batch(vec![
                make_request("eth_chainId", 2),
                disguised("personal_sign", "eth_getBalance", Id::Number(1)),
            ]),
        ];
        for packet in packets {
            let case = format!("{packet:?}");
            let error = transport.call(packet).await.expect_err("refused");
            assert!(
                matches!(error, alloy::transports::RpcError::LocalUsageError(_)),
                "{case}: {error:?}"
            );
            assert!(
                error.to_string().contains("it was not sent"),
                "{case}: {error}"
            );
        }
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        let agrees = RequestPacket::Single(make_request("eth_chainId", 1));
        transport.call(agrees).await.expect("sent");
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    /// A read packet whose answers alloy could not tell apart is refused
    /// at the public entry point with a local error and nothing sent: an
    /// empty batch, which a node answers with one error and not a batch, a
    /// read with a null id, which alloy reads the same as an answer with no
    /// id, alone or in a batch, and a batch that repeats an id, as a number
    /// or as text. A batch under the number 1 and the text "1" is sent.
    #[tokio::test]
    async fn a_read_whose_answers_could_not_be_told_apart_is_not_sent() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let url = scripted::endpoint(move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            Answer::Result(json!("0x1"))
        })
        .await;
        let mut transport = health_transport(&url);
        let text = |id: &str| Id::String(id.into());
        let packets = [
            RequestPacket::Batch(vec![]),
            RequestPacket::Single(make_request_with("eth_chainId", Id::None)),
            RequestPacket::Batch(vec![
                make_request("eth_chainId", 1),
                make_request_with("eth_blockNumber", Id::None),
            ]),
            RequestPacket::Batch(vec![
                make_request("eth_chainId", 1),
                make_request("eth_blockNumber", 2),
                make_request("eth_gasPrice", 1),
            ]),
            RequestPacket::Batch(vec![
                make_request_with("eth_chainId", text("a")),
                make_request_with("eth_blockNumber", text("a")),
            ]),
        ];
        for packet in packets {
            let case = format!("{packet:?}");
            let error = transport.call(packet).await.expect_err("refused");
            assert!(
                matches!(error, alloy::transports::RpcError::LocalUsageError(_)),
                "{case}: {error:?}"
            );
            assert!(
                error.to_string().contains("it was not sent"),
                "{case}: {error}"
            );
        }
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        let distinct = RequestPacket::Batch(vec![
            make_request("eth_chainId", 1),
            make_request_with("eth_blockNumber", text("1")),
        ]);
        transport.call(distinct).await.expect("sent");
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    /// The envelope check reads a call's body only as one JSON object with
    /// no repeated top-level key and both a method and an id: a positional
    /// array, which serde would read into the envelope's fields in order,
    /// and an object that names its method, params, id or any other key,
    /// an unknown one included, twice are no call. A key repeated inside
    /// the params is the method's business, not the envelope's.
    #[test]
    fn a_call_body_is_one_object_with_no_repeated_key() {
        let call = Envelope::parse(r#"{"jsonrpc":"2.0","method":"eth_call","params":[],"id":1}"#)
            .expect("an object");
        assert_eq!(call.method, "eth_call");
        assert_eq!(call.id, alloy::rpc::json_rpc::Id::Number(1));
        for body in [
            r#"["eth_call",1]"#,
            r#"{"method":"eth_call","method":"eth_sendTransaction","id":1}"#,
            r#"{"method":"eth_call","params":[],"params":[{"to":"0x01"}],"id":1}"#,
            r#"{"method":"eth_call","id":1,"id":2}"#,
            r#"{"jsonrpc":"2.0","jsonrpc":"2.0","method":"eth_call","id":1}"#,
            r#"{"method":"eth_call"}"#,
            r#"{"id":1}"#,
            r#"{"method":"eth_call","id":1} {}"#,
            r#"{"method":"eth_call","id":1,"x":1,"x":2}"#,
        ] {
            assert!(Envelope::parse(body).is_err(), "{body}");
        }
        for body in [
            r#"{"jsonrpc":"2.0","method":"eth_call","params":[{"a":1,"a":2}],"id":1}"#,
            r#"{"id":"x","method":"eth_call","x":1,"y":1}"#,
        ] {
            assert!(Envelope::parse(body).is_ok(), "{body}");
        }
    }

    /// A dedicated read endpoint whose circuit opened serves hedged reads
    /// again once its recovery timeout has passed: the read pool's breaker
    /// gives the read a probe, the read goes there and not to the shared
    /// pool, and its answer closes the circuit.
    #[tokio::test]
    async fn a_read_endpoint_serves_hedged_reads_again_after_recovery() {
        let hits = |answer: &'static str| {
            let requests = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&requests);
            let script = move |_: &str, _: &serde_json::Value| {
                counter.fetch_add(1, Ordering::SeqCst);
                Answer::Result(json!(answer))
            };
            (requests, script)
        };
        let (read_hits, read_script) = hits("0x1");
        let (shared_hits, shared_script) = hits("0x2");
        let transport = HftTransport::new(
            TransportConfig::builder()
                .strategy(Strategy::Hedged { fan_out: 2 })
                .request_timeout(std::time::Duration::from_secs(2))
                .read_endpoint(scripted::endpoint(read_script).await)
                .shared_endpoint(scripted::endpoint(shared_script).await)
                .build()
                .unwrap(),
        )
        .unwrap();
        let router = &transport.router;
        let opened = now_ms() - 31_000;
        for _ in 0..3 {
            router.read.record_failure(0, opened);
        }
        assert_eq!(router.read.healthy_count(), 0);

        let req = RequestPacket::Single(make_request("eth_blockNumber", 1));
        let response = router.route_request(req).await.expect("an answer");
        let answer = response.as_single().expect("a single answer");
        assert_eq!(
            answer.payload.as_success().map(|r| r.get()),
            Some(r#""0x1""#)
        );
        assert_eq!(read_hits.load(Ordering::SeqCst), 1);
        assert_eq!(shared_hits.load(Ordering::SeqCst), 0);
        assert_eq!(router.read.healthy_count(), 1);
    }

    /// A breaker that opens on the first failure, recovers after
    /// `recovery` and lets one probe through at a time.
    fn one_probe_breaker(
        recovery: std::time::Duration,
    ) -> crate::transport::config::CircuitBreakerConfig {
        crate::transport::config::CircuitBreakerConfig {
            failure_threshold: 1,
            recovery_timeout: recovery,
            half_open_max_requests: 1,
        }
    }

    /// The three ways a read is routed: round robin and latency-based on
    /// the standard path, and hedged.
    const STRATEGIES: [Strategy; 3] = [
        Strategy::RoundRobin,
        Strategy::LatencyBased,
        Strategy::Hedged { fan_out: 2 },
    ];

    /// A transport over `urls` with no read retries, whose breakers open on
    /// the first failure, recover after `recovery_ms` and let `probes`
    /// probes through at a time.
    fn breaker_transport(
        urls: Vec<String>,
        strategy: Strategy,
        recovery_ms: u64,
        probes: u32,
    ) -> HftTransport {
        let mut builder = TransportConfig::builder()
            .strategy(strategy)
            .request_timeout(std::time::Duration::from_secs(5))
            .read_retry(crate::transport::config::ReadRetryConfig {
                max_retries: 0,
                base_delay: std::time::Duration::from_millis(1),
            })
            .circuit_breaker(crate::transport::config::CircuitBreakerConfig {
                failure_threshold: 1,
                recovery_timeout: std::time::Duration::from_millis(recovery_ms),
                half_open_max_requests: probes,
            });
        for url in urls {
            builder = builder.shared_endpoint(url);
        }
        HftTransport::new(builder.build().unwrap()).unwrap()
    }

    /// Send `method` alone through `transport`'s public entry point.
    fn call_through(
        transport: &HftTransport,
        method: &'static str,
        id: u64,
    ) -> TransportFut<'static> {
        transport
            .clone()
            .call(RequestPacket::Single(make_request(method, id)))
    }

    /// The circuit states of `transport`'s endpoints.
    fn circuits(transport: &HftTransport) -> Vec<CircuitState> {
        transport
            .health_status()
            .into_iter()
            .map(|status| status.state)
            .collect()
    }

    const IDLE: CircuitState = CircuitState::HalfOpen {
        probes_in_flight: 0,
    };

    const PROBING: CircuitState = CircuitState::HalfOpen {
        probes_in_flight: 1,
    };

    /// A read sent while the circuit was closed holds no probe slot, so its
    /// late answer neither frees the slot of the probe now in flight nor
    /// closes the circuit under it: an `eth_getLogs` sent before a 503
    /// opened the circuit, declined with a 413 or answered while the
    /// recovery's `eth_blockNumber` probe waits, leaves the probe holding
    /// the only slot and the next read refused, on every strategy.
    #[tokio::test]
    async fn a_late_answer_from_before_the_circuit_opened_leaves_the_probe_deciding() {
        type Late = (&'static str, fn() -> Answer);
        let answers: [Late; 2] = [
            ("decline", || Answer::Status(413)),
            ("success", || Answer::Result(json!([]))),
        ];
        for strategy in STRATEGIES {
            for (answer, late_answer) in answers {
                let case = format!("{strategy:?}, late {answer}");
                let logs_arrived = Arc::new(Notify::new());
                let probe_arrived = Arc::new(Notify::new());
                let other_reads = Arc::new(AtomicUsize::new(0));
                let (logs, probe, others) = (
                    Arc::clone(&logs_arrived),
                    Arc::clone(&probe_arrived),
                    Arc::clone(&other_reads),
                );
                let url = scripted::endpoint(move |method, _| match method {
                    "eth_getLogs" => {
                        logs.notify_one();
                        Answer::Late(Box::new(late_answer()))
                    }
                    "eth_chainId" => Answer::Status(503),
                    "eth_blockNumber" => {
                        probe.notify_one();
                        Answer::Silence
                    }
                    _ => {
                        others.fetch_add(1, Ordering::SeqCst);
                        Answer::Result(json!("0x1"))
                    }
                })
                .await;
                let transport = breaker_transport(vec![url], strategy, 5, 1);

                let late = tokio::spawn(call_through(&transport, "eth_getLogs", 1));
                logs_arrived.notified().await;
                assert!(
                    call_through(&transport, "eth_chainId", 2).await.is_err(),
                    "{case}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                let probe = tokio::spawn(call_through(&transport, "eth_blockNumber", 3));
                probe_arrived.notified().await;
                assert_eq!(late.await.unwrap().is_ok(), answer == "success", "{case}");
                assert_eq!(circuits(&transport), [PROBING], "{case}");

                let refused = call_through(&transport, "eth_gasPrice", 4)
                    .await
                    .expect_err("the probe holds the only slot");
                assert!(
                    refused.to_string().contains("circuits open"),
                    "{case}: {refused}"
                );
                assert_eq!(other_reads.load(Ordering::SeqCst), 0, "{case}");
                probe.abort();
            }
        }
    }

    /// A late success to a read sent while the circuit was closed leaves
    /// an open circuit open: only a probe, after the recovery timeout,
    /// closes it.
    #[tokio::test]
    async fn a_late_success_leaves_an_open_circuit_open() {
        for strategy in STRATEGIES {
            let logs_arrived = Arc::new(Notify::new());
            let other_reads = Arc::new(AtomicUsize::new(0));
            let (logs, others) = (Arc::clone(&logs_arrived), Arc::clone(&other_reads));
            let url = scripted::endpoint(move |method, _| match method {
                "eth_getLogs" => {
                    logs.notify_one();
                    Answer::Late(Box::new(Answer::Result(json!([]))))
                }
                "eth_chainId" => Answer::Status(503),
                _ => {
                    others.fetch_add(1, Ordering::SeqCst);
                    Answer::Result(json!("0x1"))
                }
            })
            .await;
            let transport = breaker_transport(vec![url], strategy, 60_000, 1);

            let late = tokio::spawn(call_through(&transport, "eth_getLogs", 1));
            logs_arrived.notified().await;
            assert!(call_through(&transport, "eth_chainId", 2).await.is_err());
            assert!(late.await.unwrap().is_ok(), "{strategy:?}");
            assert!(
                matches!(circuits(&transport)[..], [CircuitState::Open { .. }]),
                "{strategy:?}: {:?}",
                circuits(&transport)
            );
            assert!(call_through(&transport, "eth_gasPrice", 3).await.is_err());
            assert_eq!(other_reads.load(Ordering::SeqCst), 0, "{strategy:?}");
        }
    }

    /// A probe the endpoint declines (HTTP 413) gives its slot back through
    /// the transport, on the standard path, the hedged path to one
    /// endpoint and the hedged fan-out: every circuit is half-open with no
    /// probe in flight after each declined probe, and the next read is
    /// served.
    #[tokio::test]
    async fn a_declined_probe_gives_its_slot_back() {
        for (strategy, endpoints) in [
            (Strategy::RoundRobin, 1),
            (Strategy::LatencyBased, 1),
            (Strategy::Hedged { fan_out: 2 }, 1),
            (Strategy::Hedged { fan_out: 2 }, 2),
        ] {
            let case = format!("{strategy:?}, {endpoints} endpoint(s)");
            let mut urls = Vec::new();
            for _ in 0..endpoints {
                urls.push(
                    scripted::endpoint(|method, _| match method {
                        "eth_chainId" => Answer::Status(503),
                        "eth_getLogs" => Answer::Status(413),
                        _ => Answer::Result(json!("0x1")),
                    })
                    .await,
                );
            }
            let transport = breaker_transport(urls, strategy, 5, 1);
            for id in 0..endpoints as u64 {
                let _ = call_through(&transport, "eth_chainId", id).await;
            }
            assert_eq!(transport.healthy_count(), 0, "{case}");

            for round in 0..3 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                assert!(
                    call_through(&transport, "eth_getLogs", 10 + round)
                        .await
                        .is_err()
                );
                assert_eq!(
                    circuits(&transport),
                    vec![IDLE; endpoints],
                    "{case}, round {round}"
                );
            }
            assert!(
                call_through(&transport, "eth_gasPrice", 20).await.is_ok(),
                "{case}"
            );
            assert!(
                circuits(&transport).contains(&CircuitState::Closed),
                "{case}: {:?}",
                circuits(&transport)
            );
        }
    }

    /// A probe whose request never reports an outcome gives its slot back
    /// when its permit drops: a caller that goes away mid-probe, on every
    /// path, and the losing request of a hedged read, which is cancelled.
    #[tokio::test]
    async fn a_probe_that_never_answers_gives_its_slot_back() {
        for (strategy, endpoints) in [
            (Strategy::RoundRobin, 1),
            (Strategy::LatencyBased, 2),
            (Strategy::Hedged { fan_out: 2 }, 1),
            (Strategy::Hedged { fan_out: 2 }, 3),
        ] {
            let case = format!("{strategy:?}, {endpoints} endpoint(s)");
            let arrived = Arc::new(AtomicUsize::new(0));
            let mut urls = Vec::new();
            for _ in 0..endpoints {
                let arrived = Arc::clone(&arrived);
                urls.push(
                    scripted::endpoint(move |method, _| match method {
                        "eth_chainId" => Answer::Status(503),
                        "eth_blockNumber" => {
                            arrived.fetch_add(1, Ordering::SeqCst);
                            Answer::Silence
                        }
                        _ => Answer::Result(json!("0x1")),
                    })
                    .await,
                );
            }
            let transport = breaker_transport(urls, strategy, 5, 1);
            for id in 0..2 * endpoints as u64 {
                let _ = call_through(&transport, "eth_chainId", id).await;
            }
            assert_eq!(transport.healthy_count(), 0, "{case}");

            for round in 0..3 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                let before = arrived.load(Ordering::SeqCst);
                let probe = tokio::spawn(call_through(&transport, "eth_blockNumber", 10));
                while arrived.load(Ordering::SeqCst) == before {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                probe.abort();
                let _ = probe.await;
                for state in circuits(&transport) {
                    assert!(
                        matches!(state, IDLE | CircuitState::Open { .. }),
                        "{case}, round {round}: {state:?}"
                    );
                }
            }
            assert!(
                call_through(&transport, "eth_gasPrice", 20).await.is_ok(),
                "{case}"
            );
        }

        let failing = Arc::new(AtomicBool::new(true));
        let slow_reads = Arc::new(AtomicUsize::new(0));
        let fast_failing = Arc::clone(&failing);
        let fast = scripted::endpoint(move |_, _| match fast_failing.load(Ordering::SeqCst) {
            true => Answer::Status(503),
            false => Answer::Result(json!("0x1")),
        })
        .await;
        let (slow_failing, slow_counter) = (Arc::clone(&failing), Arc::clone(&slow_reads));
        let slow = scripted::endpoint(move |_, _| match slow_failing.load(Ordering::SeqCst) {
            true => Answer::Status(503),
            false => {
                slow_counter.fetch_add(1, Ordering::SeqCst);
                Answer::Silence
            }
        })
        .await;
        let transport = breaker_transport(vec![fast, slow], Strategy::Hedged { fan_out: 2 }, 5, 1);
        assert!(call_through(&transport, "eth_chainId", 1).await.is_err());
        assert_eq!(transport.healthy_count(), 0);
        failing.store(false, Ordering::SeqCst);
        for round in 0..3 {
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            assert!(
                call_through(&transport, "eth_blockNumber", 10 + round)
                    .await
                    .is_ok()
            );
            assert_eq!(
                circuits(&transport),
                [CircuitState::Closed, IDLE],
                "round {round}: the loser's slot came back"
            );
            transport.router.shared.record_failure(0, now_ms());
        }
        assert_eq!(slow_reads.load(Ordering::SeqCst), 3, "probed once a round");
    }

    /// A probe of an earlier recovery cycle, declined late, frees no slot
    /// of the current cycle's probe: a read sent while closed fails 200 ms
    /// later and reopens the circuit under the first cycle's probe, and
    /// that probe's 413, arriving while the second cycle's probe waits,
    /// leaves the next read refused.
    #[tokio::test]
    async fn a_probe_of_an_earlier_cycle_frees_no_slot() {
        for strategy in STRATEGIES {
            let balance_arrived = Arc::new(Notify::new());
            let probe_arrived = Arc::new(Notify::new());
            let other_reads = Arc::new(AtomicUsize::new(0));
            let (balance, probe, others) = (
                Arc::clone(&balance_arrived),
                Arc::clone(&probe_arrived),
                Arc::clone(&other_reads),
            );
            let url = scripted::endpoint(move |method, _| match method {
                "eth_getBalance" => {
                    balance.notify_one();
                    Answer::Late(Box::new(Answer::Status(503)))
                }
                "eth_chainId" => Answer::Status(503),
                "eth_getLogs" => Answer::Late(Box::new(Answer::Status(413))),
                "eth_blockNumber" => {
                    probe.notify_one();
                    Answer::Silence
                }
                _ => {
                    others.fetch_add(1, Ordering::SeqCst);
                    Answer::Result(json!("0x1"))
                }
            })
            .await;
            let transport = breaker_transport(vec![url], strategy, 5, 1);

            let closed_read = tokio::spawn(call_through(&transport, "eth_getBalance", 1));
            balance_arrived.notified().await;
            assert!(call_through(&transport, "eth_chainId", 2).await.is_err());
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            let first_probe = tokio::spawn(call_through(&transport, "eth_getLogs", 3));
            assert!(closed_read.await.unwrap().is_err());
            assert!(
                matches!(circuits(&transport)[..], [CircuitState::Open { .. }]),
                "{strategy:?}: {:?}",
                circuits(&transport)
            );
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            let second_probe = tokio::spawn(call_through(&transport, "eth_blockNumber", 4));
            probe_arrived.notified().await;
            assert!(
                first_probe.await.unwrap().is_err(),
                "{strategy:?}: declined"
            );
            assert_eq!(circuits(&transport), [PROBING], "{strategy:?}");

            let refused = call_through(&transport, "eth_gasPrice", 5)
                .await
                .expect_err("the second cycle's probe holds the slot");
            assert!(
                refused.to_string().contains("circuits open"),
                "{strategy:?}: {refused}"
            );
            assert_eq!(other_reads.load(Ordering::SeqCst), 0, "{strategy:?}");
            second_probe.abort();
        }
    }

    /// A permit settles only the endpoint that gave it: recorded against
    /// another endpoint, its probe's success closes neither circuit, and
    /// its slot goes back to its own endpoint.
    #[test]
    fn a_permit_settles_only_its_own_endpoint() {
        let pool = EndpointPool::from_urls(
            &[
                "https://rpc1.example.com".into(),
                "https://rpc2.example.com".into(),
            ],
            crate::transport::config::CircuitBreakerConfig {
                failure_threshold: 1,
                recovery_timeout: std::time::Duration::ZERO,
                half_open_max_requests: 1,
            },
        )
        .unwrap();
        let now = now_ms();
        pool.record_failure(0, now);
        pool.record_failure(1, now);
        let _first = pool.reserve(0, now).expect("endpoint 0's probe");
        let second = pool.reserve(1, now).expect("endpoint 1's probe");

        pool.record_success(0, 1_000, second);
        let states: Vec<CircuitState> = pool.health_status().iter().map(|s| s.state).collect();
        assert_eq!(
            states,
            [
                CircuitState::HalfOpen {
                    probes_in_flight: 1
                },
                CircuitState::HalfOpen {
                    probes_in_flight: 0
                },
            ]
        );
    }

    /// A hedged selection never names one endpoint twice: an endpoint the
    /// lock-free scan took as closed, whose circuit opens before the
    /// reserving scan with no recovery timeout, is not reserved again as a
    /// probe. Endpoint 1's only probe slot is held, so the reserving scan
    /// has nothing else to take.
    #[test]
    fn a_hedge_selects_an_endpoint_once() {
        let pool = EndpointPool::from_urls(
            &[
                "https://rpc1.example.com".into(),
                "https://rpc2.example.com".into(),
            ],
            crate::transport::config::CircuitBreakerConfig {
                failure_threshold: 1,
                recovery_timeout: std::time::Duration::ZERO,
                half_open_max_requests: 1,
            },
        )
        .unwrap();
        let now = now_ms();
        pool.record_failure(1, now);
        let _held = pool.reserve(1, now).expect("endpoint 1's probe");

        let selected = pool.select_n_between(2, now, |pool| pool.record_failure(0, now));
        let selected: Vec<usize> = selected.into_iter().map(|(idx, _)| idx).collect();
        assert_eq!(selected, [0]);
        assert!(matches!(
            pool.health_status()[0].state,
            CircuitState::Open { .. }
        ));
    }

    /// A circuit that a slow failure opens starts its recovery window when
    /// the failure arrives, not when the request left: after a read that
    /// fails 200 ms or more after it was sent, by a late 503, a late
    /// JSON-RPC rate limit under HTTP 200 or by timing out at 300 ms, a
    /// recovery timeout of 100 ms still refuses the next read, on the
    /// standard path, the hedged path to one endpoint and the hedged
    /// fan-out; once the window has passed, the endpoint serves. A rate
    /// limit is an answer, which ends a fan-out at once, so the fan-out
    /// case leaves it out.
    #[tokio::test]
    async fn a_slow_failure_opens_the_circuit_when_it_arrives() {
        type Failure = (&'static str, fn() -> Answer);
        let failures: [Failure; 3] = [
            ("late 503", || Answer::Late(Box::new(Answer::Status(503)))),
            ("late rate limit", || {
                Answer::Late(Box::new(Answer::Error(
                    -32_005,
                    "rate limit exceeded".into(),
                )))
            }),
            ("timeout", || Answer::Silence),
        ];
        for (strategy, endpoints) in [
            (Strategy::RoundRobin, 1),
            (Strategy::LatencyBased, 1),
            (Strategy::Hedged { fan_out: 2 }, 1),
            (Strategy::Hedged { fan_out: 2 }, 2),
        ] {
            for (failure, fail) in failures {
                if endpoints > 1 && failure == "late rate limit" {
                    continue;
                }
                let case = format!("{strategy:?}, {endpoints} endpoint(s), {failure}");
                let hits = Arc::new(AtomicUsize::new(0));
                let mut config = TransportConfig::builder()
                    .strategy(strategy)
                    .request_timeout(std::time::Duration::from_millis(300))
                    .read_retry(crate::transport::config::ReadRetryConfig {
                        max_retries: 0,
                        base_delay: std::time::Duration::from_millis(1),
                    })
                    .circuit_breaker(one_probe_breaker(std::time::Duration::from_millis(100)));
                for _ in 0..endpoints {
                    let served = AtomicUsize::new(0);
                    let hits = Arc::clone(&hits);
                    config = config.shared_endpoint(
                        scripted::endpoint(move |_, _| {
                            hits.fetch_add(1, Ordering::SeqCst);
                            match served.fetch_add(1, Ordering::SeqCst) {
                                0 => fail(),
                                _ => Answer::Result(json!("0x1")),
                            }
                        })
                        .await,
                    );
                }
                let transport = HftTransport::new(config.build().unwrap()).unwrap();
                let read = |id| {
                    let req = RequestPacket::Single(make_request("eth_blockNumber", id));
                    transport.router.route_request(req)
                };

                let first = read(1).await;
                assert!(
                    !first.is_ok_and(|answer| answer.first_error_code().is_none()),
                    "{case}"
                );
                assert_eq!(hits.load(Ordering::SeqCst), endpoints, "{case}");
                assert!(read(2).await.is_err(), "{case}: cooling down");
                assert_eq!(
                    hits.load(Ordering::SeqCst),
                    endpoints,
                    "{case}: nothing sent"
                );
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                assert!(read(3).await.is_ok(), "{case}: recovered");
            }
        }
    }

    /// A contract that reverts with the words "rate limit" under the
    /// read's own id declined the call: the endpoint stays in service
    /// after more answers than the breaker's threshold.
    #[tokio::test]
    async fn a_revert_that_mentions_a_rate_limit_keeps_the_endpoint_in_service() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let url = scripted::endpoint(move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            Answer::Error(3, "execution reverted: rate limit".into())
        })
        .await;
        let transport = health_transport(&url);
        for _ in 0..5 {
            let req = RequestPacket::Single(make_request("eth_call", 1));
            let response = transport
                .router
                .route_request(req)
                .await
                .expect("the revert");
            assert_eq!(response.first_error_code(), Some(3));
        }
        assert_eq!(requests.load(Ordering::SeqCst), 5);
        assert_eq!(transport.healthy_count(), 1);
    }

    /// A read batch whose answers each answer their call counts against
    /// the endpoint when any answer is a rate limit, wherever it stands:
    /// invalid params and then a rate limit three times takes the endpoint
    /// out of service, where only the first error used to count. Invalid
    /// params and a success leaves it in service.
    #[tokio::test]
    async fn a_rate_limit_anywhere_in_a_read_batch_is_an_endpoint_failure() {
        for (first, second, healthy) in [
            ("invalid params", "rate limit", false),
            ("rate limit", "invalid params", false),
            ("ok", "rate limit", false),
            ("invalid params", "ok", true),
        ] {
            let url = scripted::endpoint(move |method, _| {
                match if method == "eth_getBalance" {
                    first
                } else {
                    second
                } {
                    "ok" => Answer::Result(json!("0x1")),
                    "rate limit" => Answer::Error(429, "Too Many Requests".into()),
                    _ => Answer::Error(-32_602, "invalid params".into()),
                }
            })
            .await;
            let transport = health_transport(&url);
            for _ in 0..3 {
                let batch = RequestPacket::Batch(vec![
                    make_request("eth_getBalance", 1),
                    make_request("eth_chainId", 2),
                ]);
                transport.router.route_request(batch).await.unwrap();
            }
            let case = format!("{first}, then {second}");
            assert_eq!(transport.healthy_count(), usize::from(healthy), "{case}");
        }
    }

    /// With hedged reads, an allowlisted read goes to every endpoint, a raw
    /// send goes to one of them once, so no second node can submit it from
    /// a second request, and a synchronous send or an unknown method goes
    /// to none.
    #[tokio::test]
    async fn only_an_allowlisted_read_is_hedged() {
        for (method, expected) in [
            ("eth_blockNumber", 2),
            ("eth_call", 2),
            ("eth_sendTransactionSync", 0),
            ("eth_sendRawTransactionSync", 0),
            ("eth_sendRawTransaction", 1),
            ("eth_futureMethod", 0),
        ] {
            let requests = Arc::new(AtomicUsize::new(0));
            let mut builder = TransportConfig::builder()
                .strategy(Strategy::Hedged { fan_out: 2 })
                .request_timeout(std::time::Duration::from_millis(300))
                .write_retry(WriteRetryConfig {
                    max_retries: 0,
                    base_delay: std::time::Duration::from_millis(1),
                });
            for _ in 0..2 {
                let counter = Arc::clone(&requests);
                let url = scripted::endpoint(move |_, _| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Answer::Silence
                })
                .await;
                builder = builder.shared_endpoint(url);
            }
            let client = alloy::rpc::client::RpcClient::new(
                alloy::transports::BoxTransport::new(
                    HftTransport::new(builder.build().unwrap()).unwrap(),
                ),
                false,
            );
            let error = client
                .request::<_, Value>(method, (json!({}),))
                .await
                .expect_err("no endpoint answers");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_eq!(
                requests.load(Ordering::SeqCst),
                expected,
                "{method}: {error}"
            );
            assert!(error.as_error_resp().is_none(), "{method}: {error}");
        }
    }

    /// Every read the SDK sends through alloy is on the allowlist, so it
    /// is retried after an endpoint failed to answer, and the allowlist
    /// holds nothing else: an HTTP 503 and then an error answer is two
    /// requests for each, and the methods asked are exactly
    /// [`READ_METHODS`].
    #[tokio::test]
    async fn every_read_the_sdk_sends_is_allowlisted_and_retried() {
        let asked = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = Arc::clone(&asked);
        let url = scripted::endpoint(move |method, _| {
            let mut log = log.lock().unwrap();
            let first = !log.iter().any(|m| m == method);
            log.push(method.to_owned());
            if first {
                Answer::Status(503)
            } else {
                Answer::Error(-32_000, "no".into())
            }
        })
        .await;
        let provider = alloy::providers::RootProvider::<alloy::network::Ethereum>::new(
            alloy::rpc::client::RpcClient::new(
                alloy::transports::BoxTransport::new(
                    HftTransport::new(
                        TransportConfig::builder()
                            .shared_endpoint(url)
                            .read_retry(crate::transport::config::ReadRetryConfig {
                                max_retries: 2,
                                base_delay: std::time::Duration::from_millis(1),
                            })
                            .build()
                            .unwrap(),
                    )
                    .unwrap(),
                ),
                false,
            ),
        );
        let (who, hash) = (Address::repeat_byte(0x11), B256::repeat_byte(0xbb));
        let tx = TransactionRequest::default().from(who).to(who);
        let _ = provider.get_blob_base_fee().await;
        let _ = provider.get_block_number().await;
        let _ = provider.call(tx.clone()).await;
        let _ = provider.get_chain_id().await;
        let _ = provider.estimate_gas(tx).await;
        let _ = provider
            .get_fee_history(4, BlockNumberOrTag::Latest, &[50.0])
            .await;
        let _ = provider.get_gas_price().await;
        let _ = provider.get_balance(who).await;
        let _ = provider.get_block_by_hash(hash).await;
        let _ = provider.get_block_by_number(BlockNumberOrTag::Latest).await;
        let _ = provider.get_code_at(who).await;
        let _ = provider.get_logs(&Filter::new().address(who)).await;
        let _ = provider.get_proof(who, vec![hash]).await;
        let _ = provider.get_storage_at(who, U256::ZERO).await;
        let _ = provider.get_transaction_by_hash(hash).await;
        let _ = provider.get_transaction_count(who).await;
        let _ = provider.get_transaction_receipt(hash).await;
        let _ = provider.get_max_priority_fee_per_gas().await;
        let _ = provider.syncing().await;
        let _ = provider.get_net_version().await;
        let _ = provider.get_client_version().await;
        let asked = asked.lock().unwrap().clone();
        let mut methods: Vec<&str> = asked.iter().map(String::as_str).collect();
        methods.dedup();
        assert_eq!(methods, READ_METHODS, "{asked:?}");
        for method in READ_METHODS {
            let times = asked.iter().filter(|m| m == method).count();
            assert_eq!(times, 2, "{method} was sent {times} times");
        }
    }

    /// A single `eth_sendRawTransaction` answered under another id got no
    /// answer: it is sent again as the same bytes, and when the last
    /// attempt is answered the same way, the outcome is unknown.
    #[tokio::test]
    async fn a_raw_send_answered_under_another_id_got_no_answer() {
        for (script, passed) in [
            (["text id", "text id"], false),
            (["text id", "ok"], true),
            (["ok", "text id"], true),
        ] {
            let requests = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&requests);
            let url = scripted::endpoint(move |_, _| {
                match script[counter.fetch_add(1, Ordering::SeqCst)] {
                    "text id" => Answer::ResultUnderTextId(json!(B256::repeat_byte(0xbb))),
                    _ => Answer::Result(json!(B256::repeat_byte(0xbb))),
                }
            })
            .await;
            let sent = write_client(&url, 1)
                .request::<_, B256>("eth_sendRawTransaction", (Bytes::from_static(&[2, 1]),))
                .await;
            assert_eq!(
                requests.load(Ordering::SeqCst),
                if script[0] == "ok" { 1 } else { 2 },
                "{script:?}"
            );
            match sent {
                Ok(hash) => assert!(passed && hash == B256::repeat_byte(0xbb), "{script:?}"),
                Err(error) => {
                    assert!(!passed, "{script:?}: {error}");
                    assert!(error.as_error_resp().is_none(), "{script:?}: {error}");
                    assert!(error.to_string().contains("not answered under its id"));
                }
            }
        }
    }

    /// A raw send answered under its id with a success that is no
    /// transaction hash (an error object in the result's place, or a short
    /// number) got no answer: it is sent again, and the last such answer is
    /// a transport error, never the node's rejection.
    #[tokio::test]
    async fn a_write_answered_with_no_hash_got_no_answer() {
        let error_object =
            json!({ "code": -32_000, "message": "replacement transaction underpriced" });
        for result in [error_object, json!("0x01")] {
            let requests = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&requests);
            let answer = result.clone();
            let url = scripted::endpoint(move |_, _| {
                counter.fetch_add(1, Ordering::SeqCst);
                Answer::Result(answer.clone())
            })
            .await;
            let error = write_client(&url, 1)
                .request::<_, B256>(RAW_SEND, (Bytes::from_static(&[2, 1]),))
                .await
                .expect_err("no hash, no answer");
            let case = result.to_string();
            assert_eq!(requests.load(Ordering::SeqCst), 2, "{case}");
            assert!(error.as_error_resp().is_none(), "{case}: {error}");
            assert!(
                error.to_string().contains("no transaction hash"),
                "{case}: {error}"
            );
        }
    }

    /// A provider over one endpoint that retries a write `retries` times.
    fn write_provider(
        url: &str,
        retries: u32,
    ) -> alloy::providers::RootProvider<alloy::network::Ethereum> {
        alloy::providers::RootProvider::new(write_client(url, retries))
    }

    /// A client over one endpoint that retries a write `retries` times.
    fn write_client(url: &str, retries: u32) -> alloy::rpc::client::RpcClient {
        let transport = HftTransport::new(
            TransportConfig::builder()
                .shared_endpoint(url)
                .request_timeout(std::time::Duration::from_millis(300))
                .write_retry(WriteRetryConfig {
                    max_retries: retries,
                    base_delay: std::time::Duration::from_millis(1),
                })
                .build()
                .unwrap(),
        )
        .unwrap();
        alloy::rpc::client::RpcClient::new(alloy::transports::BoxTransport::new(transport), false)
    }

    /// The width search narrows by being told no, so a declined request
    /// must cost one round trip, not one plus the read retries.
    #[tokio::test]
    async fn a_declined_request_is_sent_once() {
        let (url, served) = declining_endpoint().await;
        let error = provider_over(&url)
            .get_block_number()
            .await
            .expect_err("the endpoint declines everything");
        assert_eq!(
            served.load(Ordering::SeqCst),
            1,
            "a declined request earns the same answer every time: {error}"
        );
    }

    /// And however often it declines, it keeps serving: nothing is
    /// refused locally by an open circuit.
    #[tokio::test]
    async fn declining_every_request_keeps_the_endpoint_in_service() {
        let (url, served) = declining_endpoint().await;
        let provider = provider_over(&url);
        for _ in 0..10 {
            let _ = provider.get_block_number().await;
        }
        assert_eq!(served.load(Ordering::SeqCst), 10);
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

        pool.record_success(0, 5_000_000, ProbePermit::free());
        pool.record_success(1, 1_000_000, ProbePermit::free());
        pool.record_success(2, 3_000_000, ProbePermit::free());

        let selected = pool.select_n(2, now_ms());
        let selected: Vec<usize> = selected.into_iter().map(|(idx, _)| idx).collect();
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

        let (pool, _idx, _permit) = router.select_for(false, now_ms()).unwrap();
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

        let (pool, _idx, _permit) = router.select_for(true, now_ms()).unwrap();
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
        let (pool, _idx, _permit) = router.select_for(false, now).unwrap();
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

        let (pool, selected) = router.select_hedged(2, now);
        assert_eq!(pool.endpoint_urls()[0], "https://shared.example.com");
        assert_eq!(selected.len(), 1);
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
        pool.record_success(0, 5_000_000, ProbePermit::free());
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

        pool.record_success(0, 10_000_000, ProbePermit::free());
        pool.record_success(1, 2_000_000, ProbePermit::free());

        // Multiple selections should consistently pick endpoint 1 (lower latency)
        for _ in 0..100 {
            assert_eq!(pool.select(Strategy::LatencyBased, 1000).unwrap().0, 1);
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

        pool.record_success(0, 8_000_000, ProbePermit::free());
        pool.record_success(1, 2_000_000, ProbePermit::free());
        pool.record_success(2, 5_000_000, ProbePermit::free());

        let selected = pool.select_n(2, 1000);
        let selected: Vec<usize> = selected.into_iter().map(|(idx, _)| idx).collect();
        assert_eq!(selected, vec![1, 2]); // ordered by latency
    }
}
