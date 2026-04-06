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

use std::collections::HashMap;

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

        // Acquire nonce (lock-free atomic)
        let nonce = self.nonce_mgr.acquire();

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
