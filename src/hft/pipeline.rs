//! Transaction pipeline combining nonce management and gas caching.
//!
//! The [`TxPipeline`] is the main entry point for HFT transaction submission.
//! [`prepare`](TxPipeline::prepare) acquires a nonce and resolves gas fees
//! with **zero RPC calls** — all state is pre-cached.
//!
//! # Lifecycle
//!
//! ```text
//! prepare() → PreparedTx → sign & send → record_submission()
//!                                              ↓
//!                                    resolve() or fail()
//! ```
//!
//! Stuck transactions (older than the configured timeout) can be detected
//! with [`stuck_txs`](TxPipeline::stuck_txs) and bumped with
//! [`prepare_bump`](TxPipeline::prepare_bump).
//!
//! # Desync and recovery
//!
//! The pipeline's nonce counter is a *local mirror* of the chain's. After a
//! failed broadcast the mirrored value is unknowable: the transaction may
//! never have left, may sit in the mempool, or may already be mined — and
//! each world demands a different nonce next. No local bookkeeping can
//! distinguish them, so the pipeline never guesses. Any path that loses
//! certainty sets a `desynced` flag; while it is set, senders must not
//! prepare new transactions, and once nothing is in flight (or mid-prepare)
//! the owner re-reads the chain's count — the one authority — via
//! [`resync_nonce`](TxPipeline::resync_nonce).
//!
//! The cost is deliberate: a rare failed broadcast pauses sending until
//! in-flight work drains and costs one RPC. The alternatives measured worse —
//! rewinding or reusing the nonce spins forever when the "failed" broadcast
//! actually propagated, and doing nothing leaves a hole that silently parks
//! every later transaction (the failure mode this design replaces).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::errors::TransactionError;
use crate::hft::gas::{FeeCache, GasFees, Urgency};
use crate::hft::nonce::NonceManager;

/// A transaction request before nonce/gas are resolved.
#[derive(Debug, Clone)]
pub struct TxRequest {
    /// Destination address.
    pub to: [u8; 20],
    /// Encoded calldata.
    pub calldata: Vec<u8>,
    /// ETH value to send (usually 0 for PerpCity).
    pub value: u128,
    /// Gas limit for this operation (use [`GasLimits`](super::gas::GasLimits) constants).
    pub gas_limit: u64,
    /// Desired urgency level.
    pub urgency: Urgency,
}

/// A transaction fully prepared for signing — nonce and gas resolved.
#[derive(Debug, Clone)]
pub struct PreparedTx {
    /// The assigned nonce.
    pub nonce: u64,
    /// Gas limit.
    pub gas_limit: u64,
    /// Resolved EIP-1559 gas fees.
    pub gas_fees: GasFees,
    /// The original request.
    pub request: TxRequest,
}

/// An in-flight transaction being tracked by the pipeline.
#[derive(Debug, Clone)]
pub struct InFlightTx {
    /// The assigned nonce.
    pub nonce: u64,
    /// Transaction hash.
    pub tx_hash: [u8; 32],
    /// The original request (for resubmission).
    pub request: TxRequest,
    /// When the transaction was submitted (ms).
    pub submitted_at_ms: u64,
    /// The gas fees used.
    pub gas_fees: GasFees,
}

/// Parameters for bumping a stuck transaction's gas fees.
#[derive(Debug, Clone, Copy)]
pub struct BumpParams {
    /// Nonce of the stuck transaction (must match to replace).
    pub nonce: u64,
    /// Gas limit (same as original).
    pub gas_limit: u64,
    /// New priority fee (scaled up from original).
    pub new_max_priority_fee: u64,
    /// New fee cap (scaled up from original).
    pub new_max_fee: u64,
    /// Hash of the transaction being replaced.
    pub original_tx_hash: [u8; 32],
}

/// Pipeline configuration.
#[derive(Debug, Clone, Copy)]
pub struct PipelineConfig {
    /// Maximum concurrent in-flight transactions.
    pub max_in_flight: usize,
    /// A transaction is "stuck" if older than this (ms).
    pub stuck_timeout_ms: u64,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            max_in_flight: 16,
            stuck_timeout_ms: 30_000,
        }
    }
}

/// Transaction pipeline: zero-RPC preparation via cached nonce + gas.
///
/// Owns a [`NonceManager`] and borrows a [`FeeCache`] to prepare
/// transactions without network calls on the hot path.
#[derive(Debug)]
pub struct TxPipeline {
    nonce_mgr: NonceManager,
    config: PipelineConfig,
    in_flight: HashMap<[u8; 32], InFlightTx>,
    /// Set when a nonce's fate became unknowable (failed broadcast, receipt
    /// timeout, or a rewind blocked by concurrency). Cleared only by
    /// [`Self::resync_nonce`] — the chain is the sole authority that can
    /// re-establish where the sequence stands.
    desynced: AtomicBool,
    /// Transactions between [`Self::prepare`] and [`Self::record_submission`]
    /// (or abandonment). They hold nonces but are invisible to `in_flight`,
    /// so a resync during this window would reassign nonces out from under
    /// them — [`Self::can_resync`] refuses while any exist.
    preparing: AtomicUsize,
}

impl TxPipeline {
    /// Create a new pipeline.
    ///
    /// - `starting_nonce`: initial on-chain nonce for the sending address
    /// - `config`: pipeline limits and timeouts
    pub fn new(starting_nonce: u64, config: PipelineConfig) -> Self {
        Self {
            nonce_mgr: NonceManager::new(starting_nonce),
            config,
            in_flight: HashMap::new(),
            desynced: AtomicBool::new(false),
            preparing: AtomicUsize::new(0),
        }
    }

    /// Prepare a transaction for signing. **Zero RPC calls.**
    ///
    /// Acquires a nonce, resolves gas fees from the cache, and checks
    /// the in-flight limit. Fails fast if the cache is stale or the
    /// in-flight limit is reached.
    #[inline]
    pub fn prepare(
        &self,
        request: TxRequest,
        fee_cache: &FeeCache,
        now_ms: u64,
    ) -> std::result::Result<PreparedTx, TransactionError> {
        // Fail fast: check in-flight limit before acquiring nonce
        if self.in_flight.len() >= self.config.max_in_flight {
            tracing::warn!(
                count = self.in_flight.len(),
                max = self.config.max_in_flight,
                "too many in-flight transactions"
            );
            return Err(TransactionError::TooManyInFlight {
                count: self.in_flight.len(),
                max: self.config.max_in_flight,
            });
        }

        // Resolve gas fees from cache
        let gas_fees = fee_cache.fees_for(request.urgency, now_ms).ok_or_else(|| {
            tracing::warn!("gas cache stale or empty");
            TransactionError::GasUnavailable {
                reason: "gas cache stale or empty".into(),
            }
        })?;

        // Acquire nonce (lock-free atomic). From here until
        // record_submission or abandonment, this transaction holds a nonce
        // that nothing else can see — count it so resync waits for it.
        let nonce = self.nonce_mgr.acquire();
        self.preparing.fetch_add(1, Ordering::AcqRel);

        tracing::trace!(nonce, ?request.urgency, in_flight = self.in_flight.len(), "tx prepared");

        Ok(PreparedTx {
            nonce,
            gas_limit: request.gas_limit,
            gas_fees,
            request,
        })
    }

    /// Record a successfully submitted transaction for in-flight tracking.
    ///
    /// Call after the signed transaction has been sent to the mempool.
    pub fn record_submission(&mut self, tx_hash: [u8; 32], prepared: PreparedTx, now_ms: u64) {
        tracing::debug!(nonce = prepared.nonce, "tx submission recorded");
        self.preparing.fetch_sub(1, Ordering::AcqRel);
        self.nonce_mgr.track(prepared.nonce, tx_hash, now_ms);
        self.in_flight.insert(
            tx_hash,
            InFlightTx {
                nonce: prepared.nonce,
                tx_hash,
                request: prepared.request,
                submitted_at_ms: now_ms,
                gas_fees: prepared.gas_fees,
            },
        );
    }

    /// Mark a transaction as resolved (mined, reverted, or timed out).
    /// Removes from in-flight tracking without rewinding the nonce.
    pub fn resolve(&mut self, tx_hash: &[u8; 32]) {
        if let Some(tx) = self.in_flight.remove(tx_hash) {
            tracing::debug!(nonce = tx.nonce, "tx resolved in pipeline");
            self.nonce_mgr.confirm(tx.nonce);
        }
    }

    /// Mark a transaction as failed. Releases the nonce if possible.
    pub fn fail(&mut self, tx_hash: &[u8; 32]) {
        if let Some(tx) = self.in_flight.remove(tx_hash) {
            tracing::debug!(nonce = tx.nonce, "tx failed in pipeline");
            self.nonce_mgr.release(tx.nonce);
        }
    }

    /// Give back the nonce of a transaction that **provably never left this
    /// machine** — signing or serialization failed before any broadcast.
    ///
    /// That certainty is what makes release safe here; the broadcast-failure
    /// path must use [`Self::mark_desynced_prepared`] instead, because a
    /// "failed" broadcast may still have propagated. When a concurrent
    /// acquisition blocks the rewind the sequence is left with a hole only a
    /// chain read can repair, so the pipeline flags itself desynced.
    pub fn abandon_prepared(&self, nonce: u64) {
        self.preparing.fetch_sub(1, Ordering::AcqRel);
        if !self.nonce_mgr.release(nonce) {
            tracing::warn!(nonce, "rewind blocked; flagging desync");
            self.desynced.store(true, Ordering::Release);
        }
    }

    /// Record that a prepared transaction's broadcast failed, leaving its
    /// nonce's fate unknowable.
    ///
    /// Deliberately does **not** release the nonce. The transaction may never
    /// have left, may sit in the mempool, or may already be mined — and both
    /// rewinding and reusing spin forever in the last case (retrying a
    /// consumed nonce that the chain rejects as too low, while never trying
    /// the one it wants). Only [`Self::resync_nonce`] can adjudicate.
    pub fn mark_desynced_prepared(&self) {
        self.preparing.fetch_sub(1, Ordering::AcqRel);
        self.desynced.store(true, Ordering::Release);
    }

    /// Flag the nonce sequence as no longer provably matching the chain
    /// (e.g. a receipt timed out: the transaction may still mine later).
    pub fn mark_desynced(&self) {
        self.desynced.store(true, Ordering::Release);
    }

    /// Whether the local nonce sequence has lost certainty and sends should
    /// stop until [`Self::resync_nonce`] runs.
    pub fn is_desynced(&self) -> bool {
        self.desynced.load(Ordering::Acquire)
    }

    /// Whether a resync is safe right now: nothing in flight and nothing
    /// holding a nonce between prepare and submission. Resyncing earlier
    /// would reassign nonces already spoken for.
    pub fn can_resync(&self) -> bool {
        self.in_flight.is_empty() && self.preparing.load(Ordering::Acquire) == 0
    }

    /// Adopt the chain's transaction count as the next nonce and clear the
    /// desync flag.
    ///
    /// The count should come from `eth_getTransactionCount` with the
    /// **pending** tag, so transactions sitting in the mempool — including a
    /// "failed" broadcast that actually propagated — are counted and their
    /// nonces are not reassigned.
    pub fn resync_nonce(&self, on_chain_nonce: u64) {
        self.nonce_mgr.resync(on_chain_nonce);
        self.desynced.store(false, Ordering::Release);
        tracing::info!(nonce = on_chain_nonce, "nonce resynced from chain");
    }

    /// Return hashes of transactions that have been in-flight longer than
    /// `stuck_timeout_ms`.
    pub fn stuck_txs(&self, now_ms: u64) -> Vec<[u8; 32]> {
        self.in_flight
            .values()
            .filter(|tx| now_ms.saturating_sub(tx.submitted_at_ms) >= self.config.stuck_timeout_ms)
            .map(|tx| tx.tx_hash)
            .collect()
    }

    /// Prepare gas-bump parameters for a stuck transaction.
    ///
    /// Multiplies both priority fee and max fee by `multiplier`.
    /// Returns `None` if the transaction hash is not being tracked.
    pub fn prepare_bump(&self, tx_hash: &[u8; 32], multiplier: u64) -> Option<BumpParams> {
        let tx = self.in_flight.get(tx_hash)?;
        Some(BumpParams {
            nonce: tx.nonce,
            gas_limit: tx.request.gas_limit,
            new_max_priority_fee: tx
                .gas_fees
                .max_priority_fee_per_gas
                .saturating_mul(multiplier),
            new_max_fee: tx.gas_fees.max_fee_per_gas.saturating_mul(multiplier),
            original_tx_hash: *tx_hash,
        })
    }

    /// Number of in-flight transactions.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Access the underlying nonce manager (e.g. for resync).
    pub fn nonce_manager(&self) -> &NonceManager {
        &self.nonce_mgr
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hft::gas::GasLimits;

    const BASE_FEE: u64 = 50_000_000;
    const TIP: u64 = 1_000_000_000;

    fn test_fee_cache(now_ms: u64) -> FeeCache {
        let mut gc = FeeCache::new(5000, TIP);
        gc.update(BASE_FEE, now_ms);
        gc
    }

    fn test_request() -> TxRequest {
        TxRequest {
            to: [0xAA; 20],
            calldata: vec![0x01, 0x02, 0x03],
            value: 0,
            gas_limit: GasLimits::OPEN_TAKER,
            urgency: Urgency::Normal,
        }
    }

    #[test]
    fn prepare_assigns_nonce_and_gas() {
        let pipe = TxPipeline::new(10, PipelineConfig::default());
        let gc = test_fee_cache(0);

        let p1 = pipe.prepare(test_request(), &gc, 0).unwrap();
        assert_eq!(p1.nonce, 10);
        assert_eq!(p1.gas_limit, GasLimits::OPEN_TAKER);
        assert_eq!(p1.gas_fees.base_fee, BASE_FEE);

        let p2 = pipe.prepare(test_request(), &gc, 0).unwrap();
        assert_eq!(p2.nonce, 11);
    }

    #[test]
    fn prepare_fails_on_stale_gas() {
        let pipe = TxPipeline::new(0, PipelineConfig::default());
        let gc = test_fee_cache(0);
        // Gas cache has 5000ms TTL, query at 6000ms
        let result = pipe.prepare(test_request(), &gc, 6000);
        assert!(matches!(
            result,
            Err(TransactionError::GasUnavailable { .. })
        ));
    }

    #[test]
    fn prepare_fails_on_empty_gas() {
        let pipe = TxPipeline::new(0, PipelineConfig::default());
        let gc = FeeCache::new(5000, TIP); // never updated
        let result = pipe.prepare(test_request(), &gc, 0);
        assert!(matches!(
            result,
            Err(TransactionError::GasUnavailable { .. })
        ));
    }

    #[test]
    fn in_flight_limit_enforced() {
        let config = PipelineConfig {
            max_in_flight: 2,
            stuck_timeout_ms: 30_000,
        };
        let mut pipe = TxPipeline::new(0, config);
        let gc = test_fee_cache(0);

        // Fill up 2 slots
        for i in 0..2u8 {
            let p = pipe.prepare(test_request(), &gc, 0).unwrap();
            let mut hash = [0u8; 32];
            hash[0] = i;
            pipe.record_submission(hash, p, 0);
        }
        assert_eq!(pipe.in_flight_count(), 2);

        // Third should fail
        let result = pipe.prepare(test_request(), &gc, 0);
        assert!(matches!(
            result,
            Err(TransactionError::TooManyInFlight { count: 2, max: 2 })
        ));
    }

    #[test]
    fn resolve_removes_from_tracking_without_nonce_rewind() {
        let mut pipe = TxPipeline::new(0, PipelineConfig::default());
        let gc = test_fee_cache(0);

        let p = pipe.prepare(test_request(), &gc, 0).unwrap();
        assert_eq!(p.nonce, 0);
        let hash = [0xAA; 32];
        pipe.record_submission(hash, p, 0);
        assert_eq!(pipe.in_flight_count(), 1);

        pipe.resolve(&hash);
        assert_eq!(pipe.in_flight_count(), 0);

        // Nonce should NOT rewind — next tx gets nonce 1, not 0
        let p2 = pipe.prepare(test_request(), &gc, 0).unwrap();
        assert_eq!(p2.nonce, 1);
    }

    #[test]
    fn fail_removes_and_releases_nonce() {
        let mut pipe = TxPipeline::new(0, PipelineConfig::default());
        let gc = test_fee_cache(0);

        let p = pipe.prepare(test_request(), &gc, 0).unwrap();
        assert_eq!(p.nonce, 0);
        let hash = [0xBB; 32];
        pipe.record_submission(hash, p, 0);

        pipe.fail(&hash);
        assert_eq!(pipe.in_flight_count(), 0);
        // Nonce may or may not have been rewound depending on concurrent usage;
        // NonceManager::release only rewinds if it's the last acquired nonce.
    }

    // ── Desync state machine ────────────────────────────────────────
    //
    // After a failed broadcast the nonce's fate is locally unknowable —
    // three worlds are consistent with what was observed: the tx never
    // left, it sits in the mempool, or it mined. The pipeline never
    // guesses; it flags, gates, and lets a chain read adjudicate. Once the
    // flag is set the worlds differ only in the count the chain reports,
    // which is what these tests exercise.

    /// World 1: the transaction never left. The chain still expects the
    /// stranded nonce, and the resync hands exactly it back — the hole is
    /// filled, nothing skipped.
    #[test]
    fn resync_refills_the_hole_when_the_tx_never_left() {
        let pipe = TxPipeline::new(100, PipelineConfig::default());
        let gc = test_fee_cache(0);

        let doomed = pipe.prepare(test_request(), &gc, 0).unwrap();
        assert_eq!(doomed.nonce, 100);
        pipe.mark_desynced_prepared(); // broadcast failed

        assert!(pipe.is_desynced());
        assert!(pipe.can_resync());
        pipe.resync_nonce(100); // chain: nothing landed, count still 100

        assert!(!pipe.is_desynced());
        let next = pipe.prepare(test_request(), &gc, 0).unwrap();
        assert_eq!(next.nonce, 100, "the hole must be refilled");
    }

    /// Worlds 2 and 3: the "failed" broadcast actually propagated (mempool
    /// or mined — identical under the pending tag). The doomed nonce must
    /// NEVER be retried: this is the spin both release-based designs died
    /// on, retrying a consumed nonce forever while never trying the one the
    /// chain wanted.
    #[test]
    fn resync_steps_past_a_nonce_the_chain_consumed() {
        let pipe = TxPipeline::new(100, PipelineConfig::default());
        let gc = test_fee_cache(0);

        let doomed = pipe.prepare(test_request(), &gc, 0).unwrap();
        assert_eq!(doomed.nonce, 100);
        pipe.mark_desynced_prepared(); // "failed" — but it landed

        pipe.resync_nonce(101); // chain (pending tag): 100 is taken

        let next = pipe.prepare(test_request(), &gc, 0).unwrap();
        assert_eq!(next.nonce, 101, "the consumed nonce must never be retried");
    }

    /// While transactions are in flight, resync must wait: the chain's
    /// count cannot yet account for them, and adopting it would reassign
    /// their nonces.
    #[test]
    fn resync_is_gated_on_in_flight_draining() {
        let mut pipe = TxPipeline::new(0, PipelineConfig::default());
        let gc = test_fee_cache(0);

        let submitted = pipe.prepare(test_request(), &gc, 0).unwrap();
        pipe.record_submission([0xAA; 32], submitted, 0);
        let _doomed = pipe.prepare(test_request(), &gc, 0).unwrap();
        pipe.mark_desynced_prepared();

        assert!(pipe.is_desynced());
        assert!(!pipe.can_resync(), "in-flight tx blocks resync");

        pipe.resolve(&[0xAA; 32]); // receipt arrives
        assert!(pipe.can_resync(), "drained — now the chain read is safe");
    }

    /// A transaction between prepare() and record_submission() holds a
    /// nonce that in_flight cannot see. Resyncing in that window would
    /// hand its nonce to someone else.
    #[test]
    fn resync_is_gated_on_the_prepare_window() {
        let mut pipe = TxPipeline::new(0, PipelineConfig::default());
        let gc = test_fee_cache(0);

        let mid_prepare = pipe.prepare(test_request(), &gc, 0).unwrap();
        pipe.mark_desynced(); // some other tx timed out, say

        assert!(
            !pipe.can_resync(),
            "a prepared-but-unsubmitted tx blocks resync"
        );

        pipe.record_submission([0xBB; 32], mid_prepare, 0);
        assert!(!pipe.can_resync(), "now it is in flight instead");
        pipe.resolve(&[0xBB; 32]);
        assert!(pipe.can_resync());
    }

    /// Signing failure is the one case where release IS safe — the tx
    /// provably never left the machine. Single-flight, the rewind succeeds
    /// and no desync is flagged: zero-cost recovery.
    #[test]
    fn an_abandoned_prepare_rewinds_cleanly_when_single_flight() {
        let pipe = TxPipeline::new(7, PipelineConfig::default());
        let gc = test_fee_cache(0);

        let p = pipe.prepare(test_request(), &gc, 0).unwrap();
        assert_eq!(p.nonce, 7);
        pipe.abandon_prepared(p.nonce); // signing failed

        assert!(!pipe.is_desynced(), "local failure, local repair");
        let next = pipe.prepare(test_request(), &gc, 0).unwrap();
        assert_eq!(next.nonce, 7, "nonce handed straight back");
    }

    /// When concurrency blocks the rewind, the abandoned nonce is a hole —
    /// and holes are the chain's to repair, so the pipeline must flag
    /// rather than shrug.
    #[test]
    fn an_abandoned_prepare_flags_desync_when_rewind_is_blocked() {
        let pipe = TxPipeline::new(0, PipelineConfig::default());
        let gc = test_fee_cache(0);

        let first = pipe.prepare(test_request(), &gc, 0).unwrap();
        let _second = pipe.prepare(test_request(), &gc, 0).unwrap();
        pipe.abandon_prepared(first.nonce); // rewind blocked by second

        assert!(pipe.is_desynced(), "an unrepairable hole must be visible");
    }

    #[test]
    fn stuck_txs_detection() {
        let config = PipelineConfig {
            max_in_flight: 16,
            stuck_timeout_ms: 10_000,
        };
        let mut pipe = TxPipeline::new(0, config);
        let gc = test_fee_cache(0);

        // Submit at t=0
        let p1 = pipe.prepare(test_request(), &gc, 0).unwrap();
        pipe.record_submission([0x01; 32], p1, 0);

        // Submit at t=5000
        let gc2 = test_fee_cache(5000);
        let p2 = pipe.prepare(test_request(), &gc2, 5000).unwrap();
        pipe.record_submission([0x02; 32], p2, 5000);

        // At t=10_000: first tx is stuck (10s old), second is not (5s old)
        let stuck = pipe.stuck_txs(10_000);
        assert_eq!(stuck.len(), 1);
        assert_eq!(stuck[0], [0x01; 32]);

        // At t=15_000: both stuck
        let stuck = pipe.stuck_txs(15_000);
        assert_eq!(stuck.len(), 2);
    }

    #[test]
    fn prepare_bump_scales_fees() {
        let mut pipe = TxPipeline::new(0, PipelineConfig::default());
        let gc = test_fee_cache(0);

        let p = pipe.prepare(test_request(), &gc, 0).unwrap();
        let original_priority = p.gas_fees.max_priority_fee_per_gas;
        let original_max = p.gas_fees.max_fee_per_gas;
        let hash = [0xCC; 32];
        pipe.record_submission(hash, p, 0);

        let bump = pipe.prepare_bump(&hash, 2).unwrap();
        assert_eq!(bump.new_max_priority_fee, original_priority * 2);
        assert_eq!(bump.new_max_fee, original_max * 2);
        assert_eq!(bump.original_tx_hash, hash);
    }

    #[test]
    fn prepare_bump_unknown_tx_returns_none() {
        let pipe = TxPipeline::new(0, PipelineConfig::default());
        assert!(pipe.prepare_bump(&[0xFF; 32], 2).is_none());
    }

    #[test]
    fn resolve_unknown_tx_is_noop() {
        let mut pipe = TxPipeline::new(0, PipelineConfig::default());
        pipe.resolve(&[0xFF; 32]); // should not panic
        assert_eq!(pipe.in_flight_count(), 0);
    }

    #[test]
    fn urgency_propagates_through_prepare() {
        let pipe = TxPipeline::new(0, PipelineConfig::default());
        let gc = test_fee_cache(0);

        let mut req = test_request();
        req.urgency = Urgency::Critical;
        let p = pipe.prepare(req, &gc, 0).unwrap();

        // Critical: 4*base + 5*tip
        assert_eq!(p.gas_fees.max_fee_per_gas, 4 * BASE_FEE + 5 * TIP);
    }

    #[test]
    fn full_lifecycle() {
        let config = PipelineConfig {
            max_in_flight: 4,
            stuck_timeout_ms: 30_000,
        };
        let mut pipe = TxPipeline::new(100, config);
        let gc = test_fee_cache(0);

        // Prepare → submit → confirm
        let p1 = pipe.prepare(test_request(), &gc, 0).unwrap();
        assert_eq!(p1.nonce, 100);
        pipe.record_submission([0x01; 32], p1, 0);

        let p2 = pipe.prepare(test_request(), &gc, 100).unwrap();
        assert_eq!(p2.nonce, 101);
        pipe.record_submission([0x02; 32], p2, 100);

        assert_eq!(pipe.in_flight_count(), 2);

        pipe.resolve(&[0x01; 32]);
        assert_eq!(pipe.in_flight_count(), 1);

        pipe.fail(&[0x02; 32]);
        assert_eq!(pipe.in_flight_count(), 0);
    }
}
