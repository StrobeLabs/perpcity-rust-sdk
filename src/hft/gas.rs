//! EIP-1559 gas fee caching with urgency-based scaling.
//!
//! Pre-computed gas limits eliminate `estimateGas` RPC calls on the hot path.
//! The [`GasCache`] stores the latest base fee from block headers and
//! computes EIP-1559 fees scaled by [`Urgency`].
//!
//! # Example
//!
//! ```
//! use perpcity_sdk::hft::gas::{GasCache, Urgency, GasLimits};
//!
//! let mut cache = GasCache::new(2_000, 1_000_000_000);
//! cache.update(50_000_000, 1000); // base_fee from block header, at t=1000ms
//!
//! let fees = cache.fees_for(Urgency::Normal, 1500).unwrap(); // within TTL
//! assert!(fees.max_fee_per_gas >= fees.max_priority_fee_per_gas);
//! ```

/// Pre-empirically derived gas limits for PerpCity operations.
///
/// Each limit includes ~20% margin over observed mainnet usage.
#[derive(Debug, Clone, Copy)]
pub struct GasLimits;

impl GasLimits {
    /// ERC-20 `approve` call.
    pub const APPROVE: u64 = 60_000;
    /// Open a taker position (market order).
    pub const OPEN_TAKER: u64 = 700_000;
    /// Open a maker position (range order).
    pub const OPEN_MAKER: u64 = 800_000;
    /// Close any position.
    pub const CLOSE_POSITION: u64 = 600_000;
    /// Adjust position notional (add/remove exposure).
    pub const ADJUST_NOTIONAL: u64 = 500_000;
    /// Adjust position margin (add/remove collateral).
    pub const ADJUST_MARGIN: u64 = 500_000;
}

/// Transaction urgency level, controlling EIP-1559 fee scaling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Urgency {
    /// `maxFee = baseFee + priorityFee`. Cost-optimized, may be slow.
    Low,
    /// `maxFee = 2 * baseFee + priorityFee`. Standard EIP-1559 headroom.
    Normal,
    /// `maxFee = 3 * baseFee + 2 * priorityFee`. Faster inclusion.
    High,
    /// `maxFee = 4 * baseFee + 5 * priorityFee`. For liquidations / time-critical.
    Critical,
}

/// EIP-1559 gas fees ready to attach to a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GasFees {
    /// The block base fee this was computed from (wei).
    pub base_fee: u64,
    /// Miner tip (wei).
    pub max_priority_fee_per_gas: u64,
    /// Fee cap (wei). Always ≥ `base_fee + max_priority_fee_per_gas`.
    pub max_fee_per_gas: u64,
    /// Timestamp (ms) when the underlying base fee was observed.
    pub updated_at_ms: u64,
}

/// Cached EIP-1559 gas fees with TTL-based staleness detection.
///
/// Updated from block headers (typically via subscription or polling).
/// All methods that check freshness take an explicit `now_ms` parameter
/// for deterministic testing.
#[derive(Debug)]
pub struct GasCache {
    current: Option<GasFees>,
    ttl_ms: u64,
    default_priority_fee: u64,
}

impl GasCache {
    /// Create a new cache.
    ///
    /// - `ttl_ms`: how long cached fees are valid (2000 = 2 Base L2 blocks)
    /// - `default_priority_fee`: miner tip in wei (e.g. 1_000_000_000 = 1 gwei)
    pub fn new(ttl_ms: u64, default_priority_fee: u64) -> Self {
        Self {
            current: None,
            ttl_ms,
            default_priority_fee,
        }
    }

    /// Update the cache from a new block header's base fee.
    pub fn update(&mut self, base_fee: u64, now_ms: u64) {
        self.current = Some(GasFees {
            base_fee,
            max_priority_fee_per_gas: self.default_priority_fee,
            // Store the "Normal" urgency as the default cached value
            max_fee_per_gas: 2u64
                .saturating_mul(base_fee)
                .saturating_add(self.default_priority_fee),
            updated_at_ms: now_ms,
        });
    }

    /// Check if the cache has valid (non-stale) fees.
    #[inline]
    pub fn is_valid(&self, now_ms: u64) -> bool {
        self.current
            .map(|f| now_ms.saturating_sub(f.updated_at_ms) < self.ttl_ms)
            .unwrap_or(false)
    }

    /// Get the raw cached fees if still within TTL.
    #[inline]
    pub fn get(&self, now_ms: u64) -> Option<&GasFees> {
        self.current
            .as_ref()
            .filter(|f| now_ms.saturating_sub(f.updated_at_ms) < self.ttl_ms)
    }

    /// Override the cache TTL (milliseconds).
    ///
    /// Use this when gas is managed externally (e.g. a shared poller
    /// distributing base fees via [`PerpClient::set_base_fee`]). Set the
    /// TTL to match the poller's cadence with some headroom.
    pub fn set_ttl(&mut self, ttl_ms: u64) {
        self.ttl_ms = ttl_ms;
    }

    /// Return the current cached base fee (ignoring TTL).
    #[inline]
    pub fn base_fee(&self) -> Option<u64> {
        self.current.map(|f| f.base_fee)
    }

    /// Compute fees scaled for the given [`Urgency`], or `None` if stale/empty.
    ///
    /// Fee formulas:
    /// - **Low**: `base + priority`
    /// - **Normal**: `2*base + priority`
    /// - **High**: `3*base + 2*priority`
    /// - **Critical**: `4*base + 5*priority`
    #[inline]
    pub fn fees_for(&self, urgency: Urgency, now_ms: u64) -> Option<GasFees> {
        let base = self.get(now_ms)?;
        let bf = base.base_fee;
        let pf = self.default_priority_fee;

        let (max_fee, priority) = match urgency {
            Urgency::Low => (bf.saturating_add(pf), pf),
            Urgency::Normal => (2u64.saturating_mul(bf).saturating_add(pf), pf),
            Urgency::High => (
                3u64.saturating_mul(bf)
                    .saturating_add(2u64.saturating_mul(pf)),
                2u64.saturating_mul(pf),
            ),
            Urgency::Critical => (
                4u64.saturating_mul(bf)
                    .saturating_add(5u64.saturating_mul(pf)),
                5u64.saturating_mul(pf),
            ),
        };

        Some(GasFees {
            base_fee: bf,
            max_priority_fee_per_gas: priority,
            max_fee_per_gas: max_fee,
            updated_at_ms: base.updated_at_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 50_000_000; // 50 Mwei ~ typical Base L2
    const TIP: u64 = 1_000_000_000; // 1 gwei

    fn cache_with_fees(now_ms: u64) -> GasCache {
        let mut c = GasCache::new(2000, TIP);
        c.update(BASE, now_ms);
        c
    }

    #[test]
    fn empty_cache_is_invalid() {
        let c = GasCache::new(2000, TIP);
        assert!(!c.is_valid(0));
        assert!(c.get(0).is_none());
        assert!(c.fees_for(Urgency::Normal, 0).is_none());
    }

    #[test]
    fn update_makes_cache_valid() {
        let c = cache_with_fees(1000);
        assert!(c.is_valid(1000));
        assert!(c.is_valid(2999)); // within 2000ms TTL
    }

    #[test]
    fn cache_expires_after_ttl() {
        let c = cache_with_fees(1000);
        assert!(c.is_valid(2999));
        assert!(!c.is_valid(3000)); // exactly at TTL boundary
        assert!(!c.is_valid(5000));
    }

    #[test]
    fn low_urgency_fees() {
        let c = cache_with_fees(0);
        let f = c.fees_for(Urgency::Low, 0).unwrap();
        assert_eq!(f.max_fee_per_gas, BASE + TIP);
        assert_eq!(f.max_priority_fee_per_gas, TIP);
        assert_eq!(f.base_fee, BASE);
    }

    #[test]
    fn normal_urgency_fees() {
        let c = cache_with_fees(0);
        let f = c.fees_for(Urgency::Normal, 0).unwrap();
        assert_eq!(f.max_fee_per_gas, 2 * BASE + TIP);
        assert_eq!(f.max_priority_fee_per_gas, TIP);
    }

    #[test]
    fn high_urgency_fees() {
        let c = cache_with_fees(0);
        let f = c.fees_for(Urgency::High, 0).unwrap();
        assert_eq!(f.max_fee_per_gas, 3 * BASE + 2 * TIP);
        assert_eq!(f.max_priority_fee_per_gas, 2 * TIP);
    }

    #[test]
    fn critical_urgency_fees() {
        let c = cache_with_fees(0);
        let f = c.fees_for(Urgency::Critical, 0).unwrap();
        assert_eq!(f.max_fee_per_gas, 4 * BASE + 5 * TIP);
        assert_eq!(f.max_priority_fee_per_gas, 5 * TIP);
    }

    #[test]
    fn urgency_ordering() {
        let c = cache_with_fees(0);
        let low = c.fees_for(Urgency::Low, 0).unwrap().max_fee_per_gas;
        let normal = c.fees_for(Urgency::Normal, 0).unwrap().max_fee_per_gas;
        let high = c.fees_for(Urgency::High, 0).unwrap().max_fee_per_gas;
        let critical = c.fees_for(Urgency::Critical, 0).unwrap().max_fee_per_gas;
        assert!(low < normal);
        assert!(normal < high);
        assert!(high < critical);
    }

    #[test]
    fn fees_for_stale_returns_none() {
        let c = cache_with_fees(0);
        assert!(c.fees_for(Urgency::Normal, 3000).is_none());
    }

    #[test]
    fn update_replaces_old_fees() {
        let mut c = cache_with_fees(0);
        c.update(100_000_000, 5000); // new base fee
        let f = c.fees_for(Urgency::Low, 5000).unwrap();
        assert_eq!(f.base_fee, 100_000_000);
    }

    #[test]
    fn saturating_arithmetic_on_huge_values() {
        let mut c = GasCache::new(2000, u64::MAX / 2);
        c.update(u64::MAX / 2, 0);
        // Should not panic, uses saturating math
        let f = c.fees_for(Urgency::Critical, 0).unwrap();
        assert_eq!(f.max_fee_per_gas, u64::MAX);
    }

    #[test]
    fn preserves_timestamp_across_urgency() {
        let c = cache_with_fees(42);
        for urgency in [
            Urgency::Low,
            Urgency::Normal,
            Urgency::High,
            Urgency::Critical,
        ] {
            let f = c.fees_for(urgency, 42).unwrap();
            assert_eq!(f.updated_at_ms, 42);
        }
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn gas_limits_are_reasonable() {
        // Ensure limits are in a sane range (not accidentally 0 or astronomical)
        assert!(GasLimits::APPROVE > 20_000 && GasLimits::APPROVE < 200_000);
        assert!(GasLimits::OPEN_TAKER > 200_000 && GasLimits::OPEN_TAKER < 2_000_000);
        assert!(GasLimits::CLOSE_POSITION > 100_000 && GasLimits::CLOSE_POSITION < 2_000_000);
        // Maker is more expensive than taker (more Uniswap V4 work)
        assert!(GasLimits::OPEN_MAKER > GasLimits::OPEN_TAKER);
    }
}
