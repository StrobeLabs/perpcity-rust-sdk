//! Lock-free nonce management for high-frequency transaction submission.
//!
//! The [`NonceManager`] uses an [`AtomicU64`] for O(1) lock-free nonce
//! acquisition and a [`Mutex`]-protected [`HashMap`] for tracking pending
//! (in-flight) transactions. The hot path — [`NonceManager::acquire`] — never takes a lock.
//!
//! # Example
//!
//! ```
//! use perpcity_rust_sdk::hft::nonce::NonceManager;
//!
//! let mgr = NonceManager::new(42);
//! let n1 = mgr.acquire();
//! let n2 = mgr.acquire();
//! assert_eq!(n1, 42);
//! assert_eq!(n2, 43);
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// A transaction that has been submitted but not yet confirmed on-chain.
#[derive(Debug, Clone, Copy)]
pub struct PendingTx {
    /// The transaction hash.
    pub tx_hash: [u8; 32],
    /// The nonce used for this transaction.
    pub nonce: u64,
    /// Timestamp (ms) when the transaction was submitted.
    pub submitted_at_ms: u64,
}

/// Lock-free nonce manager for HFT bots.
///
/// Designed so that [`acquire`](Self::acquire) is a single atomic
/// `fetch_add` — no lock, no RPC, no allocation.
///
/// Thread-safe: `acquire` uses atomics; `track`/`confirm`/`release`
/// take a mutex only over the pending map.
#[derive(Debug)]
pub struct NonceManager {
    next: AtomicU64,
    pending: Mutex<HashMap<u64, PendingTx>>,
}

impl NonceManager {
    /// Create a new manager starting at `on_chain_nonce`.
    ///
    /// Typically initialized with the result of `eth_getTransactionCount`.
    pub fn new(on_chain_nonce: u64) -> Self {
        Self {
            next: AtomicU64::new(on_chain_nonce),
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Atomically acquire the next nonce. **Lock-free, O(1).**
    ///
    /// Each call returns a unique, monotonically increasing nonce.
    #[inline]
    pub fn acquire(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// Peek at the next nonce without incrementing.
    #[inline]
    pub fn peek(&self) -> u64 {
        self.next.load(Ordering::Relaxed)
    }

    /// Record a submitted transaction for tracking.
    ///
    /// Call this after successfully sending a transaction to the mempool.
    pub fn track(&self, nonce: u64, tx_hash: [u8; 32], submitted_at_ms: u64) {
        let tx = PendingTx {
            tx_hash,
            nonce,
            submitted_at_ms,
        };
        self.pending.lock().unwrap().insert(nonce, tx);
    }

    /// Mark a nonce as confirmed (transaction mined). Removes from pending.
    ///
    /// Returns the [`PendingTx`] if it was being tracked, or `None`.
    pub fn confirm(&self, nonce: u64) -> Option<PendingTx> {
        self.pending.lock().unwrap().remove(&nonce)
    }

    /// Release a nonce that was never submitted (e.g. signing failed).
    ///
    /// **Only rewinds the counter if `nonce` is still the most recently
    /// acquired value** (i.e. `next == nonce + 1`). This prevents gaps
    /// while avoiding interference with concurrently acquired nonces.
    ///
    /// Also removes the nonce from pending tracking if present.
    pub fn release(&self, nonce: u64) -> bool {
        self.pending.lock().unwrap().remove(&nonce);
        self.next
            .compare_exchange(nonce + 1, nonce, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    /// Reset to a known on-chain state. Clears all pending transactions.
    ///
    /// Call this after detecting nonce desync (e.g. another wallet
    /// instance submitted transactions, or after a node failover).
    pub fn resync(&self, on_chain_nonce: u64) {
        self.next.store(on_chain_nonce, Ordering::Relaxed);
        self.pending.lock().unwrap().clear();
    }

    /// Number of pending (unconfirmed) transactions.
    pub fn pending_count(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    /// Get a snapshot of all pending transactions.
    pub fn pending_snapshot(&self) -> Vec<PendingTx> {
        self.pending.lock().unwrap().values().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_is_monotonic() {
        let mgr = NonceManager::new(0);
        let nonces: Vec<u64> = (0..100).map(|_| mgr.acquire()).collect();
        for i in 1..nonces.len() {
            assert_eq!(nonces[i], nonces[i - 1] + 1);
        }
    }

    #[test]
    fn acquire_starts_at_on_chain_nonce() {
        let mgr = NonceManager::new(999);
        assert_eq!(mgr.acquire(), 999);
        assert_eq!(mgr.acquire(), 1000);
    }

    #[test]
    fn peek_does_not_advance() {
        let mgr = NonceManager::new(10);
        assert_eq!(mgr.peek(), 10);
        assert_eq!(mgr.peek(), 10);
        assert_eq!(mgr.acquire(), 10);
        assert_eq!(mgr.peek(), 11);
    }

    #[test]
    fn track_and_confirm() {
        let mgr = NonceManager::new(0);
        let n = mgr.acquire();
        mgr.track(n, [0xAA; 32], 1000);
        assert_eq!(mgr.pending_count(), 1);

        let tx = mgr.confirm(n).unwrap();
        assert_eq!(tx.nonce, 0);
        assert_eq!(tx.tx_hash, [0xAA; 32]);
        assert_eq!(tx.submitted_at_ms, 1000);
        assert_eq!(mgr.pending_count(), 0);
    }

    #[test]
    fn confirm_unknown_nonce_returns_none() {
        let mgr = NonceManager::new(0);
        assert!(mgr.confirm(42).is_none());
    }

    #[test]
    fn release_rewinds_if_last_acquired() {
        let mgr = NonceManager::new(5);
        let n = mgr.acquire(); // n=5, next=6
        assert!(mgr.release(n)); // next back to 5
        assert_eq!(mgr.peek(), 5);
        assert_eq!(mgr.acquire(), 5); // reuse
    }

    #[test]
    fn release_does_not_rewind_if_another_acquired_after() {
        let mgr = NonceManager::new(5);
        let n1 = mgr.acquire(); // 5
        let _n2 = mgr.acquire(); // 6, next=7
        assert!(!mgr.release(n1)); // n1+1=6 != next=7
        assert_eq!(mgr.peek(), 7); // unchanged
    }

    #[test]
    fn release_removes_from_pending() {
        let mgr = NonceManager::new(0);
        let n = mgr.acquire();
        mgr.track(n, [0xBB; 32], 500);
        assert_eq!(mgr.pending_count(), 1);
        mgr.release(n);
        assert_eq!(mgr.pending_count(), 0);
    }

    #[test]
    fn resync_resets_everything() {
        let mgr = NonceManager::new(0);
        for _ in 0..5 {
            let n = mgr.acquire();
            mgr.track(n, [0x11; 32], 100);
        }
        assert_eq!(mgr.pending_count(), 5);
        assert_eq!(mgr.peek(), 5);

        mgr.resync(100);
        assert_eq!(mgr.peek(), 100);
        assert_eq!(mgr.pending_count(), 0);
    }

    #[test]
    fn pending_snapshot_returns_all() {
        let mgr = NonceManager::new(0);
        mgr.track(0, [0x01; 32], 100);
        mgr.track(1, [0x02; 32], 200);
        mgr.track(2, [0x03; 32], 300);

        let snap = mgr.pending_snapshot();
        assert_eq!(snap.len(), 3);
    }

    #[test]
    fn struct_sizes() {
        // PendingTx must fit in a cache line (64 bytes)
        assert_eq!(std::mem::size_of::<PendingTx>(), 48);
        assert_eq!(std::mem::align_of::<PendingTx>(), 8);
    }

    #[test]
    fn concurrent_acquire_no_duplicates() {
        use std::sync::Arc;
        use std::thread;

        let mgr = Arc::new(NonceManager::new(0));
        let mut handles = Vec::new();

        for _ in 0..8 {
            let mgr = Arc::clone(&mgr);
            handles.push(thread::spawn(move || {
                (0..1000).map(|_| mgr.acquire()).collect::<Vec<_>>()
            }));
        }

        let mut all: Vec<u64> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), 8000, "no duplicate nonces across 8 threads");
        assert_eq!(all[0], 0);
        assert_eq!(all[all.len() - 1], 7999);
    }
}
