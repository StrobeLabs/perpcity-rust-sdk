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
//! - **Read/write classification**: only a method on an allowlist of
//!   reads without side effects (`eth_call`, `eth_getLogs`,
//!   `eth_blockNumber` and the other reads the SDK sends) is retried or
//!   hedged.
//!   A read is retried when the endpoint failed to answer, never when it
//!   declined the request. Every other method, a new or unknown one
//!   included, is a write. Only a single `eth_sendRawTransaction` is
//!   retried, as the same signed bytes, after an error answer and after
//!   no answer: the bytes are one transaction, so a resend cannot make a
//!   second, but the outcome of an earlier attempt is unknown, so a
//!   caller reconciling a send should trust the receipt rather than the
//!   error
//!   ([`TransactionError::tx_hash`](crate::errors::TransactionError::tx_hash)).
//!   Every other write (`eth_sendTransaction`, a synchronous send, an
//!   unknown method, a batch that holds a write) is sent once, and any
//!   failure of it, an error answer included, is a transport error: the
//!   outcome is unknown
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

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use alloy::primitives::B256;
use alloy::rpc::json_rpc::{
    RequestPacket, Response, ResponsePacket, ResponsePayload, SerializedRequest,
};
use alloy::transports::{TransportError, TransportErrorKind, TransportFut};
use tower::Service;

use super::config::{Strategy, TransportConfig};
use super::fault::{Fault, fault, is_rate_limit, is_rejection};
use super::health::{CircuitState, EndpointHealth, EndpointStatus, Reserved};

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
    fn release_probe(&self) {
        let mut h = self.health.lock().unwrap();
        h.release_probe();
        self.atomic_state
            .store(pack_state(h.state()), Ordering::Relaxed);
    }

    /// Record an answer that declined the request. Updates Mutex state +
    /// atomic mirrors.
    #[inline]
    fn record_answered(&self) {
        let mut h = self.health.lock().unwrap();
        h.record_answered();
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
/// slot comes back. [`Self::resolved`] says an outcome was recorded, and
/// the outcome already settled the slot.
#[must_use = "dropping the permit releases the endpoint's probe slot; that is usually what you want, but say so"]
#[derive(Debug)]
pub struct ProbePermit<'a> {
    /// `None` once resolved, or when the circuit was closed and no slot
    /// was taken.
    held: Option<&'a ManagedEndpoint>,
}

impl ProbePermit<'_> {
    /// A permit over a closed circuit: nothing was reserved, so nothing
    /// has to be given back. Cheap enough for the lock-free path.
    fn free() -> Self {
        Self { held: None }
    }

    /// The request was sent and its outcome recorded, which resolved the
    /// slot; the permit has nothing left to give back.
    pub fn resolved(mut self) {
        self.held = None;
    }
}

impl Drop for ProbePermit<'_> {
    fn drop(&mut self) {
        if let Some(endpoint) = self.held.take() {
            endpoint.release_probe();
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
    fn reserve(&self, idx: usize, now_ms: u64) -> Option<ProbePermit<'_>> {
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
            Reserved::Probe => Some(ProbePermit {
                held: Some(endpoint),
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
    /// Slow path: no Closed endpoints available. Lock each non-Closed endpoint
    /// and call `is_callable()` which may transition Open→HalfOpen. Only entered
    /// when circuit breakers have tripped (error condition, rare).
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

        // Only reserve elsewhere if the closed endpoints cannot fill the
        // fan-out: a reservation this function hands back is a probe slot
        // spent on a request that never happens.
        if candidates.len() < n && any_non_closed {
            for idx in 0..self.endpoints.len() {
                if self.endpoints[idx].atomic_state.load(Ordering::Relaxed) & TAG_MASK == TAG_CLOSED
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

    /// Record a successful request on endpoint `idx`.
    pub fn record_success(&self, idx: usize, latency_ns: u64) {
        self.endpoints[idx].record_success(latency_ns);
    }

    /// Record a failed request on endpoint `idx`.
    pub fn record_failure(&self, idx: usize, now_ms: u64) {
        self.endpoints[idx].record_failure(now_ms);
    }

    /// Record what a failed request says about endpoint `idx`.
    ///
    /// A request the endpoint declined is not held against it — it
    /// answered, and every endpoint would decline the same request — but
    /// the answer is still evidence it is alive, which ends its failure
    /// streak and releases the probe slot a half-open request took. See
    /// [`Fault`](super::fault::Fault).
    pub fn record_error(&self, idx: usize, now_ms: u64, error: &TransportError) {
        match fault(error) {
            Fault::Endpoint => self.record_failure(idx, now_ms),
            Fault::Request => self.endpoints[idx].record_answered(),
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

/// The methods this transport retries and hedges: reads without side
/// effects, the SDK's own and the common ones next to them. Every other
/// method, a new or unknown one included, is a write.
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

/// The one write that is sent again, as the same signed bytes, and whose
/// error answer is the node's rejection.
const RAW_SEND: &str = "eth_sendRawTransaction";

/// Whether `req` holds a write: any call to a method that is not in
/// [`READ_METHODS`].
///
/// A write is never hedged. Only a single `eth_sendRawTransaction` is
/// sent again, as the same signed bytes; every other request that holds
/// a write is sent once ([`sent_once`]).
fn is_write_method(req: &RequestPacket) -> bool {
    match req {
        RequestPacket::Single(call) => is_write_method_name(call.method()),
        RequestPacket::Batch(calls) => calls.iter().any(|c| is_write_method_name(c.method())),
    }
}

fn is_write_method_name(method: &str) -> bool {
    !READ_METHODS.contains(&method)
}

/// Whether `req` is a write that is sent once, whatever its answer and
/// whatever [`WriteRetryConfig`](super::config::WriteRetryConfig) says:
/// every write but a single `eth_sendRawTransaction`. A call the node
/// signs picks its nonce at the node, so a resend after a lost reply can
/// be a second transaction, and an unknown method may do the same.
fn sent_once(req: &RequestPacket) -> bool {
    match req {
        RequestPacket::Single(call) => {
            call.method() != RAW_SEND && is_write_method_name(call.method())
        }
        RequestPacket::Batch(calls) => calls.iter().any(|c| is_write_method_name(c.method())),
    }
}

/// Whether `method` answers with a transaction hash, so that any other
/// success is no answer to it. A synchronous send
/// (`eth_sendTransactionSync`, `eth_sendRawTransactionSync`) answers with
/// a receipt and an unknown method with anything, so their success passes
/// as it came.
fn answers_with_hash(method: &str) -> bool {
    matches!(
        method,
        RAW_SEND | "eth_sendTransaction" | "personal_sendTransaction"
    )
}

/// The error that refuses the write `req` before it is sent, when its
/// answer could not be told apart from another: a call with a null id,
/// which alloy reads the same as an answer with no id, or a batch whose
/// ids repeat. `None` when every call can be answered by its own id.
fn uncorrelated_write(req: &RequestPacket) -> Option<TransportError> {
    let calls: &[SerializedRequest] = match req {
        RequestPacket::Single(call) => std::slice::from_ref(call),
        RequestPacket::Batch(calls) => calls,
    };
    let reason = if calls.iter().any(|call| call.id().is_none()) {
        "a request that holds a write has a null id"
    } else if calls
        .iter()
        .enumerate()
        .any(|(i, call)| calls[..i].iter().any(|earlier| earlier.id() == call.id()))
    {
        "a batch that holds a write repeats a request id"
    } else {
        return None;
    };
    Some(TransportError::local_usage_str(&format!(
        "{reason}, so its answer could not be told apart: it was not sent"
    )))
}

/// The transport error that replaces `response` to the write `req` when
/// the response is no answer to it, so the outcome of the write is
/// unknown; `None` when it is one.
///
/// A single write is answered only by one response with its own id,
/// compared as typed (the number `0` is not the string `"0"`), on the
/// error path and the success path alike, and a success of a method that
/// answers with a hash ([`answers_with_hash`]) only by a transaction hash.
/// Only a single `eth_sendRawTransaction` is answered by an error: an
/// error answer to any other write is a transport error. A batch that
/// holds a write is answered only by a batch with one success for each
/// call, each by its id, and a transaction hash for each write that
/// answers with one: the transport never reports an error inside such a
/// batch as the node's rejection of a write, so any error in it, a lone
/// error object for the whole batch included, is a transport error too.
fn unanswered_write(req: &RequestPacket, response: &ResponsePacket) -> Option<TransportError> {
    let reason = match (req, response) {
        (RequestPacket::Single(call), ResponsePacket::Single(answer))
            if answer.id == *call.id() =>
        {
            match &answer.payload {
                ResponsePayload::Failure(error) if call.method() != RAW_SEND => format!(
                    "the write was answered {} {:?}, and only an eth_sendRawTransaction is \
                     rejected by an error answer",
                    error.code, error.message
                ),
                payload if is_answer_to(call.method(), payload) => return None,
                _ => "the write was answered with a result that is no transaction hash".to_owned(),
            }
        }
        (RequestPacket::Single(call), _) => {
            format!("the write was not answered under its id {}", call.id())
        }
        (RequestPacket::Batch(_), ResponsePacket::Single(_)) => {
            "a batch that holds a write was answered with one response, not a batch".to_owned()
        }
        (RequestPacket::Batch(calls), ResponsePacket::Batch(answers)) => {
            if let Some(error) = response.iter_errors().next() {
                format!(
                    "a batch that holds a write was answered {} {:?}",
                    error.code, error.message
                )
            } else if !answers_each_once(calls, answers) {
                "a batch that holds a write was not answered once for each call by its id"
                    .to_owned()
            } else if answers.iter().any(|answer| {
                calls.iter().any(|call| {
                    call.id() == &answer.id && !is_answer_to(call.method(), &answer.payload)
                })
            }) {
                "a write in a batch was answered with a result that is no transaction hash"
                    .to_owned()
            } else {
                return None;
            }
        }
    };
    Some(TransportErrorKind::custom_str(&format!(
        "{reason}: the outcome of the write is unknown"
    )))
}

/// Whether `payload` answers a call to `method`: an error, or a success
/// whose result reads as a transaction hash when `method` answers with
/// one ([`answers_with_hash`]), so that the caller decodes it as one and
/// never mistakes an error object in its place for the node's error
/// answer.
fn is_answer_to(method: &str, payload: &ResponsePayload) -> bool {
    match payload {
        ResponsePayload::Success(result) if answers_with_hash(method) => {
            serde_json::from_str::<B256>(result.get()).is_ok()
        }
        ResponsePayload::Success(_) | ResponsePayload::Failure(_) => true,
    }
}

/// Whether `answers` answer each of `calls` once, by its id.
fn answers_each_once(calls: &[SerializedRequest], answers: &[Response]) -> bool {
    answers.len() == calls.len()
        && calls.iter().all(|call| {
            answers
                .iter()
                .filter(|answer| answer.id == *call.id())
                .count()
                == 1
        })
}

/// The transport error that replaces `error`, the transport's failure on
/// a write. A body that does not read as a JSON-RPC response is no
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
    /// Only a read on [`READ_METHODS`] retries, on any transport or RPC
    /// error that is not the endpoint declining it, and is hedged. A
    /// single `eth_sendRawTransaction` retries on an error answer (e.g.
    /// `-32003 insufficient funds` from a stale read replica) and on no
    /// answer, always as the same signed bytes; its last explicit
    /// rejection ([`is_rejection`]) is returned as the node gave it only
    /// when no earlier attempt may have left the transaction with a node,
    /// and comes back as a transport error otherwise. Every other write
    /// (any method not on the list, or a batch that holds one) is sent
    /// once ([`sent_once`]). A write with a null or repeated request id is
    /// refused before it is sent ([`uncorrelated_write`]). A write's
    /// response that is no answer to it ([`unanswered_write`]: another id,
    /// another shape, a success that is no transaction hash, an error to
    /// any write but a single raw send, or for a batch any error) is a
    /// transport error, the write's outcome unknown, and so is a body that
    /// is no JSON-RPC response ([`undecoded_write`]).
    async fn route_request(
        self: &Arc<Self>,
        req: RequestPacket,
    ) -> Result<ResponsePacket, TransportError> {
        let is_write = is_write_method(&req);
        if is_write && let Some(error) = uncorrelated_write(&req) {
            return Err(error);
        }
        let (max_attempts, base_delay) = if sent_once(&req) {
            (1, std::time::Duration::ZERO)
        } else if is_write {
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
        let mut may_be_held = false;

        for attempt in 0..max_attempts {
            // Per attempt, not per call: a retry sequence spans its
            // backoffs, and a circuit opened on the last attempt would
            // otherwise be stamped with the time of the first — starting
            // its recovery window early.
            let now_ms = now_ms();
            let Some((pool, idx, permit)) = self.select_for(is_write, now_ms) else {
                tracing::error!("all RPC endpoints unavailable (circuits open)");
                return Err(TransportError::local_usage_str(
                    "all RPC endpoints unavailable (circuits open)",
                ));
            };

            let start = Instant::now();
            let mut transport = pool.transport(idx);

            // Whose fault a write's error answer was, when it is turned
            // into a transport error: the endpoint answered, so only a
            // rate limit counts against its health.
            let mut answered = None;
            // Apply tower timeout
            let result = match tokio::time::timeout(timeout, transport.call(req.clone())).await {
                Ok(Ok(response)) if is_write => match unanswered_write(&req, &response) {
                    Some(error) => {
                        answered = response.iter_errors().next().map(|e| {
                            if is_rate_limit(e) {
                                Fault::Endpoint
                            } else {
                                Fault::Request
                            }
                        });
                        Ok(Err(error))
                    }
                    None => Ok(Ok(response)),
                },
                Ok(Err(error)) if is_write => Ok(Err(undecoded_write(error))),
                result => result,
            };

            match result {
                Ok(Ok(response)) => {
                    // A single raw send's error answer is retried as the
                    // same signed bytes; a write sent once returns it on
                    // its only attempt.
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
                        pool.record_success(idx, latency_ns);
                        permit.resolved();
                        return Ok(response);
                    }
                }
                Ok(Err(e)) => {
                    match answered {
                        Some(Fault::Request) => pool.endpoints[idx].record_answered(),
                        Some(Fault::Endpoint) => pool.record_failure(idx, now_ms),
                        None => pool.record_error(idx, now_ms, &e),
                    }
                    permit.resolved();
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
                    // is going to get anyway. Writes keep their own retry
                    // policy, which turns on whether the transaction
                    // could have landed.
                    if !is_write && fault(&e) == Fault::Request {
                        return Err(e);
                    }
                    may_be_held = true;
                    last_err = Some(e);
                }
                Err(_timeout) => {
                    pool.record_failure(idx, now_ms);
                    permit.resolved();
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
        let selected = pool.select_n(fan_out, now_ms);

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
            let result = tokio::time::timeout(timeout, transport.call(req)).await;

            return match result {
                Ok(Ok(resp)) => {
                    pool.record_success(idx, start.elapsed().as_nanos() as u64);
                    permit.resolved();
                    Ok(resp)
                }
                Ok(Err(e)) => {
                    pool.record_error(idx, now_ms, &e);
                    permit.resolved();
                    Err(e)
                }
                Err(_) => {
                    pool.record_failure(idx, now_ms);
                    permit.resolved();
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
                let result = tokio::time::timeout(timeout, transport.call(req_clone)).await;
                let result = match result {
                    Ok(r) => r,
                    Err(_) => Err(TransportError::local_usage_str("request timed out")),
                };
                (idx, result, start)
            });
        }

        /// Hand back the permit of the endpoint that just reported, so its
        /// outcome — not its drop — settles the probe slot.
        fn resolve(permits: &mut Vec<(usize, ProbePermit<'_>)>, idx: usize) {
            if let Some(at) = permits.iter().position(|&(held, _)| held == idx) {
                permits.swap_remove(at).1.resolved();
            }
        }

        let mut last_err = None;

        while let Some(join_result) = join_set.join_next().await {
            match join_result {
                Ok((idx, Ok(response), start)) => {
                    let latency_ns = start.elapsed().as_nanos() as u64;
                    pool.record_success(idx, latency_ns);
                    resolve(&mut permits, idx);
                    // Cancel remaining in-flight requests — saves RPC rate limits.
                    // JoinSet::drop also aborts, but explicit abort_all is clearer.
                    // The cancelled requests' permits drop with `permits`.
                    join_set.abort_all();
                    return Ok(response);
                }
                Ok((idx, Err(e), _start)) => {
                    pool.record_error(idx, now_ms, &e);
                    resolve(&mut permits, idx);
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
    use alloy::primitives::{Address, Bytes, U256};
    use serde_json::{Value, json};

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

    #[test]
    fn classify_write_methods() {
        let read = RequestPacket::Single(make_request("eth_getBlockByNumber", 1));
        assert!(!is_write_method(&read));

        let write = RequestPacket::Single(make_request("eth_sendRawTransaction", 2));
        assert!(is_write_method(&write));
        assert!(!sent_once(&write));

        for method in [
            "eth_sendTransaction",
            "personal_sendTransaction",
            "eth_sendTransactionSync",
            "eth_sendRawTransactionSync",
            "eth_sendRawTransactionConditional",
            "eth_subscribe",
            "eth_futureMethod",
            "",
        ] {
            let other = RequestPacket::Single(make_request(method, 3));
            assert!(is_write_method(&other), "{method}");
            assert!(sent_once(&other), "{method}");
        }
        for method in READ_METHODS {
            let read = RequestPacket::Single(make_request(method, 4));
            assert!(!is_write_method(&read), "{method}");
            assert!(!sent_once(&read), "{method}");
        }
    }

    #[test]
    fn classify_batch_with_write() {
        let batch = RequestPacket::Batch(vec![
            make_request("eth_getBalance", 1),
            make_request("eth_sendRawTransaction", 2),
        ]);
        assert!(is_write_method(&batch));
        assert!(sent_once(&batch));

        let reads = RequestPacket::Batch(vec![
            make_request("eth_getBalance", 1),
            make_request("eth_chainId", 2),
        ]);
        assert!(!is_write_method(&reads));
        assert!(!sent_once(&reads));

        let unknown = RequestPacket::Batch(vec![
            make_request("eth_getBalance", 1),
            make_request("eth_futureMethod", 2),
        ]);
        assert!(is_write_method(&unknown));
        assert!(sent_once(&unknown));
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

        pool.record_success(0, 10_000_000); // 10ms
        pool.record_success(1, 1_000_000); // 1ms

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
        use alloy::transports::TransportErrorKind;

        let pool =
            EndpointPool::from_urls(&["https://rpc1.example.com".into()], Default::default())
                .unwrap();
        let now = now_ms();

        let declined = TransportErrorKind::http_error(400, String::new());
        for _ in 0..10 {
            pool.record_error(0, now, &declined);
        }
        assert_eq!(
            pool.select(Strategy::LatencyBased, now).map(|(idx, _)| idx),
            Some(0),
            "an endpoint that declines requests is still working"
        );

        // An endpoint that cannot answer still loses its circuit.
        let overloaded = TransportErrorKind::http_error(503, String::new());
        for _ in 0..3 {
            pool.record_error(0, now, &overloaded);
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
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

    #[allow(unused_imports)]
    use alloy::providers::Provider as _;

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

    /// The answer to a write counts only under the write's own id,
    /// compared as typed, on the error path and the success path alike,
    /// and a success only as a transaction hash; a batch that holds a
    /// write counts as answered only by a batch with one success for each
    /// call, by its id, and a hash for each write, so any error in it (a
    /// read's, a write's, or one error object for the whole batch) leaves
    /// the outcome unknown.
    #[test]
    fn a_write_is_answered_only_under_its_own_id() {
        use alloy::rpc::json_rpc::Id;
        const U: &str = r#"{"code":-32000,"message":"replacement transaction underpriced"}"#;
        let hash = format!(r#""{}""#, B256::repeat_byte(0xbb));
        let ok_with =
            |id: &str, result: &str| format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{result}}}"#);
        let ok = |id: &str| ok_with(id, &hash);
        let err = |id: &str| format!(r#"{{"jsonrpc":"2.0","id":{id},"error":{U}}}"#);
        let single =
            |method: &'static str, id: Id| RequestPacket::Single(make_request_with(method, id));
        let raw = single("eth_sendRawTransaction", Id::Number(0));
        let raw_text = single("eth_sendRawTransaction", Id::String("0".into()));
        let signed = single("eth_sendTransaction", Id::Number(0));
        let signed_text = single("eth_sendTransaction", Id::String("0".into()));
        let personal = single("personal_sendTransaction", Id::Number(0));
        let sync = single("eth_sendTransactionSync", Id::Number(0));
        let raw_sync = single("eth_sendRawTransactionSync", Id::Number(0));
        let unknown = single("eth_futureMethod", Id::Number(0));
        let receipt = r#"{"transactionHash":"0x01","status":"0x1"}"#;
        let writes: RequestPacket = [
            make_request("eth_sendRawTransaction", 1),
            make_request("eth_sendRawTransaction", 2),
        ]
        .into_iter()
        .collect();
        let one: RequestPacket = [make_request("eth_sendRawTransaction", 1)]
            .into_iter()
            .collect();
        let mixed: RequestPacket = [
            make_request("eth_chainId", 1),
            make_request("eth_sendRawTransaction", 2),
        ]
        .into_iter()
        .collect();
        let rows = [
            (&raw, ok("0"), true),
            (&raw, ok(r#""0""#), false),
            (&raw_text, ok(r#""0""#), true),
            (&raw_text, ok("0"), false),
            (&raw, err("0"), true),
            (&raw, err(r#""0""#), false),
            (&raw, ok("null"), false),
            (&raw, ok("1"), false),
            (&raw, format!("[{}]", ok("0")), false),
            (&raw, ok_with("0", U), false),
            (&raw, ok_with("0", r#""0x01""#), false),
            (&signed, ok_with("0", U), false),
            (&personal, ok_with("0", "null"), false),
            (&signed, ok("0"), true),
            (&signed, ok(r#""0""#), false),
            (&signed_text, ok(r#""0""#), true),
            (&signed_text, ok("0"), false),
            (&signed, err("0"), false),
            (&sync, ok_with("0", receipt), true),
            (&sync, ok("0"), true),
            (&sync, ok_with("0", U), true),
            (&sync, ok(r#""0""#), false),
            (&sync, err("0"), false),
            (&sync, err("null"), false),
            (&raw_sync, ok_with("0", receipt), true),
            (&raw_sync, err("0"), false),
            (&raw_sync, ok_with("1", receipt), false),
            (&unknown, ok_with("0", "null"), true),
            (&unknown, err("0"), false),
            (&unknown, ok_with("1", "null"), false),
            (&personal, ok(r#""0""#), false),
            (&personal, ok("0"), true),
            (&writes, format!("[{},{}]", ok("2"), ok("1")), true),
            (&writes, format!("[{},{}]", ok(r#""1""#), ok("2")), false),
            (&writes, format!("[{}]", ok("1")), false),
            (&writes, format!("[{},{}]", ok("1"), ok("1")), false),
            (&writes, format!("[{},{}]", ok("1"), ok("3")), false),
            (
                &writes,
                format!("[{},{},{}]", ok("1"), ok("2"), ok("2")),
                false,
            ),
            (
                &writes,
                format!("[{},{},{}]", ok("1"), ok("2"), ok("3")),
                false,
            ),
            (&writes, format!("[{},{}]", ok("1"), err("2")), false),
            (&writes, format!("[{},{}]", err("1"), err("2")), false),
            (&writes, err("null"), false),
            (&writes, err("1"), false),
            (&writes, ok("1"), false),
            (&writes, format!("[{},{}]", ok("1"), ok_with("2", U)), false),
            (&one, format!("[{}]", ok("1")), true),
            (&one, ok("1"), false),
            (&one, format!("[{}]", ok_with("1", U)), false),
            (&mixed, format!("[{},{}]", ok("1"), ok("2")), true),
            (
                &mixed,
                format!("[{},{}]", ok_with("1", r#""0x1""#), ok("2")),
                true,
            ),
            (
                &mixed,
                format!("[{},{}]", ok("1"), ok_with("2", r#""0x1""#)),
                false,
            ),
            (&mixed, format!("[{},{}]", err("1"), ok("2")), false),
            (&mixed, format!("[{},{}]", ok("1"), err("2")), false),
            (&mixed, err("2"), false),
            (&mixed, err("null"), false),
        ];
        for (req, body, answered) in rows {
            let response: ResponsePacket = serde_json::from_str(&body).unwrap();
            let unknown = unanswered_write(req, &response);
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

    /// A batch that holds a write is sent once, whatever its answer, and
    /// any error in its answer fails the whole batch as a transport error,
    /// never as a node's rejection of one of its writes; a batch that every
    /// call answered with a success passes as it came.
    #[tokio::test]
    async fn a_batch_that_holds_a_write_is_sent_once_and_never_rejected() {
        const U: &str = "replacement transaction underpriced";
        let lone = json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32_000, "message": U } });
        for (first, second, read) in [
            (U, U, false),
            (U, "ok", false),
            ("ok", U, false),
            ("ok", "ok", false),
            ("ok", U, true),
            ("read error", "ok", true),
            ("ok", "ok", true),
            ("lone error", "ok", true),
            ("lone error", "ok", false),
            ("HTTP 503", "ok", false),
            ("no answer", "ok", false),
        ] {
            let lone = lone.clone();
            let requests = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&requests);
            // Every batch asks `a` first, so `a` counts the requests.
            let url = scripted::endpoint(move |method, params| {
                let answer = if params.get(0).is_some_and(|raw| raw == "0x0202") {
                    second
                } else {
                    counter.fetch_add(1, Ordering::SeqCst);
                    first
                };
                match answer {
                    "ok" if method == "eth_chainId" => Answer::Result(json!("0x1")),
                    "ok" => Answer::Result(json!(B256::repeat_byte(0xbb))),
                    "read error" => Answer::Error(-32_603, "internal error".into()),
                    "lone error" => Answer::Body(lone.clone()),
                    "HTTP 503" => Answer::Status(503),
                    "no answer" => Answer::Silence,
                    answer => Answer::Error(-32_000, answer.into()),
                }
            })
            .await;
            let client = write_client(&url, 3);
            let mut batch = client.new_batch();
            let a = if read {
                batch.add_call::<_, Value>("eth_chainId", &()).unwrap()
            } else {
                batch
                    .add_call::<_, Value>("eth_sendRawTransaction", &(Bytes::from_static(&[2, 1]),))
                    .unwrap()
            };
            let b = batch
                .add_call::<_, Value>("eth_sendRawTransaction", &(Bytes::from_static(&[2, 2]),))
                .unwrap();
            let sent = batch.send().await;
            let (a, b) = (a.await, b.await);
            let case = format!("{first}, {second}, read={read}");
            assert_eq!(requests.load(Ordering::SeqCst), 1, "{case}");
            if (first, second) == ("ok", "ok") {
                sent.unwrap();
                assert!(a.is_ok() && b.is_ok(), "{case}: {a:?} {b:?}");
                continue;
            }
            let error = sent.expect_err("the whole batch fails");
            assert!(error.as_error_resp().is_none(), "{case}: {error}");
            for answer in [&a, &b] {
                let error = answer
                    .as_ref()
                    .expect_err("no call of the batch is answered");
                assert!(error.as_error_resp().is_none(), "{case}: {error}");
            }
        }
    }

    /// Every write but a single `eth_sendRawTransaction` is sent once: a
    /// call the node signs (`eth_sendTransaction`,
    /// `personal_sendTransaction`, and their synchronous forms) picks its
    /// nonce at the node, so after an HTTP 503 or no answer a resend could
    /// be signed with the next nonce as a second trade, and an unknown
    /// method may do as much. A lost reply, an error answer under the
    /// call's own id and a success under the id written as a string (`"0"`
    /// for `0`) are all transport errors, the outcome unknown; a success
    /// under the id passes as it came.
    #[tokio::test]
    async fn every_write_but_a_raw_send_is_sent_once() {
        const U: &str = "replacement transaction underpriced";
        for method in [
            "eth_sendTransaction",
            "personal_sendTransaction",
            "eth_sendTransactionSync",
            "eth_sendRawTransactionSync",
            "eth_sendRawTransactionConditional",
            "eth_futureMethod",
        ] {
            for first in ["HTTP 503", "no answer", U, "ok", "text id"] {
                let requests = Arc::new(AtomicUsize::new(0));
                let counter = Arc::clone(&requests);
                let url = scripted::endpoint(move |asked, _| {
                    assert_eq!(asked, method);
                    if counter.fetch_add(1, Ordering::SeqCst) > 0 {
                        return Answer::Error(-32_000, U.into());
                    }
                    match first {
                        "HTTP 503" => Answer::Status(503),
                        "no answer" => Answer::Silence,
                        "ok" => Answer::Result(json!({ "status": "0x1" })),
                        "text id" => Answer::ResultUnderTextId(json!(B256::repeat_byte(0xbb))),
                        answer => Answer::Error(-32_000, answer.into()),
                    }
                })
                .await;
                let call =
                    json!({ "from": Address::repeat_byte(0x11), "to": Address::repeat_byte(0x22) });
                let sent = write_client(&url, 3)
                    .request::<_, Value>(method, (call,))
                    .await;
                let case = format!("{method}, {first}");
                assert_eq!(requests.load(Ordering::SeqCst), 1, "{case}: {sent:?}");
                if first == "ok" && !answers_with_hash(method) {
                    assert_eq!(sent.unwrap(), json!({ "status": "0x1" }), "{case}");
                    continue;
                }
                let error = sent.expect_err("one attempt, no success");
                assert!(error.as_error_resp().is_none(), "{case}: {error}");
            }
        }
    }

    /// `send_transaction_sync` sends `eth_sendTransactionSync`, which the
    /// node signs: after a lost reply it is not sent again, so a node's
    /// rejection of a second attempt can never come back under another
    /// id as the answer; the caller gets a transport error.
    #[tokio::test]
    async fn a_lost_sync_send_is_not_sent_again() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let url = scripted::endpoint(move |method, _| {
            assert_eq!(method, "eth_sendTransactionSync");
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                Answer::Silence
            } else {
                Answer::Error(-32_000, "replacement transaction underpriced".into())
            }
        })
        .await;
        let tx = alloy::rpc::types::TransactionRequest::default()
            .from(Address::repeat_byte(0x11))
            .to(Address::repeat_byte(0x22));
        let error = write_provider(&url, 3)
            .send_transaction_sync(tx)
            .await
            .expect_err("the reply was lost");
        assert_eq!(requests.load(Ordering::SeqCst), 1, "{error}");
        assert!(error.as_error_resp().is_none(), "{error}");
    }

    /// A write whose answer could not be told apart is refused before it
    /// is sent: a null id, which alloy reads the same as an answer with no
    /// id, and a batch that holds a write and repeats an id. A read with a
    /// null id is sent as before.
    #[tokio::test]
    async fn a_write_with_a_null_or_repeated_id_is_not_sent() {
        use alloy::rpc::json_rpc::Id;
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let url = scripted::endpoint(move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            Answer::Result(json!(B256::repeat_byte(0xbb)))
        })
        .await;
        let transport = HftTransport::new(
            TransportConfig::builder()
                .shared_endpoint(&url)
                .build()
                .unwrap(),
        )
        .unwrap();
        let refused: [RequestPacket; 5] = [
            RequestPacket::Single(make_request_with("eth_sendRawTransaction", Id::None)),
            RequestPacket::Single(make_request_with("eth_sendTransactionSync", Id::None)),
            RequestPacket::Single(make_request_with("eth_futureMethod", Id::None)),
            RequestPacket::Batch(vec![
                make_request("eth_sendRawTransaction", 1),
                make_request("eth_sendRawTransaction", 1),
            ]),
            RequestPacket::Batch(vec![
                make_request("eth_chainId", 1),
                make_request_with("eth_sendRawTransaction", Id::None),
            ]),
        ];
        for req in refused {
            let error = transport
                .router
                .route_request(req.clone())
                .await
                .expect_err("refused");
            assert!(
                error.to_string().contains("it was not sent"),
                "{req:?}: {error}"
            );
        }
        assert_eq!(requests.load(Ordering::SeqCst), 0);

        let reads = RequestPacket::Batch(vec![
            make_request("eth_chainId", 1),
            make_request("eth_chainId", 1),
        ]);
        let read = RequestPacket::Single(make_request_with("eth_chainId", Id::None));
        for req in [reads, read] {
            let _ = transport.router.route_request(req).await;
        }
        // One count for each call: two in the batch, and the single read.
        assert_eq!(requests.load(Ordering::SeqCst), 3);
    }

    /// With hedged reads, an allowlisted read goes to every endpoint, and
    /// a write, a synchronous send and an unknown method included, goes to
    /// one of them once, so no second node can submit a second
    /// transaction.
    #[tokio::test]
    async fn only_an_allowlisted_read_is_hedged() {
        for (method, expected) in [
            ("eth_blockNumber", 2),
            ("eth_call", 2),
            ("eth_sendTransactionSync", 1),
            ("eth_sendRawTransactionSync", 1),
            ("eth_sendRawTransaction", 1),
            ("eth_futureMethod", 1),
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
        use alloy::eips::BlockNumberOrTag;
        use alloy::rpc::types::{Filter, TransactionRequest};
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

    /// A write answered under its id with a success that is no transaction
    /// hash (an error object in the result's place, or a short number) got
    /// no answer: a transport error, never the node's rejection, whether
    /// the write is sent again or sent once.
    #[tokio::test]
    async fn a_write_answered_with_no_hash_got_no_answer() {
        let error_object =
            json!({ "code": -32_000, "message": "replacement transaction underpriced" });
        for (method, result) in [
            ("eth_sendRawTransaction", error_object.clone()),
            ("eth_sendRawTransaction", json!("0x01")),
            ("eth_sendTransaction", error_object),
        ] {
            let requests = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&requests);
            let answer = result.clone();
            let url = scripted::endpoint(move |_, _| {
                counter.fetch_add(1, Ordering::SeqCst);
                Answer::Result(answer.clone())
            })
            .await;
            let error = write_client(&url, 1)
                .request::<_, B256>(method, (Bytes::from_static(&[2, 1]),))
                .await
                .expect_err("no hash, no answer");
            let case = format!("{method}, {result}");
            let attempts = if method == "eth_sendRawTransaction" {
                2
            } else {
                1
            };
            assert_eq!(requests.load(Ordering::SeqCst), attempts, "{case}");
            assert!(error.as_error_resp().is_none(), "{case}: {error}");
            assert!(
                error.to_string().contains("no transaction hash"),
                "{case}: {error}"
            );
        }
    }

    /// A batch of one write is answered only by a batch: the same success
    /// as one object, not in an array, is a transport error.
    #[tokio::test]
    async fn a_batch_of_one_write_is_answered_only_by_a_batch() {
        for alone in [false, true] {
            let url = scripted::endpoint(move |_, _| {
                let hash = json!(B256::repeat_byte(0xbb));
                if alone {
                    Answer::Alone(hash)
                } else {
                    Answer::Result(hash)
                }
            })
            .await;
            let client = write_client(&url, 3);
            let mut batch = client.new_batch();
            let sent = batch
                .add_call::<_, B256>("eth_sendRawTransaction", &(Bytes::from_static(&[2, 1]),))
                .unwrap();
            let result = batch.send().await;
            let sent = sent.await;
            if alone {
                let error = result.expect_err("one object answers no batch");
                assert!(error.as_error_resp().is_none(), "{error}");
                assert!(error.to_string().contains("not a batch"), "{error}");
                let error = sent.expect_err("the write is not answered");
                assert!(error.as_error_resp().is_none(), "{error}");
            } else {
                result.unwrap();
                assert_eq!(sent.unwrap(), B256::repeat_byte(0xbb));
            }
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

        pool.record_success(0, 5_000_000);
        pool.record_success(1, 1_000_000);
        pool.record_success(2, 3_000_000);

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

        pool.record_success(0, 8_000_000);
        pool.record_success(1, 2_000_000);
        pool.record_success(2, 5_000_000);

        let selected = pool.select_n(2, 1000);
        let selected: Vec<usize> = selected.into_iter().map(|(idx, _)| idx).collect();
        assert_eq!(selected, vec![1, 2]); // ordered by latency
    }
}
