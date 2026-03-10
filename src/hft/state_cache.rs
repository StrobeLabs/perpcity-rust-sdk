//! Multi-layer TTL-based state cache for frequently-read on-chain data.
//!
//! Two TTL tiers match the data's expected rate of change on Base L2:
//!
//! | Layer | TTL | Data | Why |
//! |---|---|---|---|
//! | **Slow** | 60 s | Fees, bounds | Change only via governance |
//! | **Fast** | 2 s (1 block) | Mark prices, funding rates, USDC balance | Change every block |
//!
//! All methods take an explicit `now_ts` (Unix seconds) for deterministic
//! testing — no hidden clock dependencies.
//!
//! # Example
//!
//! ```
//! use perpcity_sdk::hft::state_cache::{StateCache, StateCacheConfig, CachedFees};
//!
//! let mut cache = StateCache::new(StateCacheConfig::default());
//! let addr = [0xAA; 20];
//! let fees = CachedFees {
//!     creator_fee: 0.001,
//!     insurance_fee: 0.0005,
//!     lp_fee: 0.003,
//!     liquidation_fee: 0.01,
//! };
//! cache.put_fees(addr, fees, 1000);
//! assert!(cache.get_fees(&addr, 1050).is_some()); // within 60s TTL
//! assert!(cache.get_fees(&addr, 1061).is_none()); // expired
//! ```

use std::collections::HashMap;

/// A cached value with an expiration timestamp.
#[derive(Debug, Clone, Copy)]
pub struct CachedValue<T> {
    /// The cached value.
    pub value: T,
    /// Unix timestamp (seconds) when this entry expires.
    pub expires_at: u64,
}

impl<T> CachedValue<T> {
    /// Check if the cached value is still valid at the given time.
    #[inline]
    pub fn is_valid(&self, now_ts: u64) -> bool {
        now_ts < self.expires_at
    }
}

/// Cached fee configuration for a perpetual market.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CachedFees {
    /// Creator fee (e.g. 0.001 = 0.1%).
    pub creator_fee: f64,
    /// Insurance fund fee.
    pub insurance_fee: f64,
    /// LP fee.
    pub lp_fee: f64,
    /// Liquidation fee.
    pub liquidation_fee: f64,
}

/// Cached position/leverage bounds for a perpetual market.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CachedBounds {
    /// Minimum margin in USDC.
    pub min_margin: f64,
    /// Minimum taker leverage.
    pub min_taker_leverage: f64,
    /// Maximum taker leverage.
    pub max_taker_leverage: f64,
    /// Liquidation margin ratio for takers.
    pub liquidation_taker_ratio: f64,
}

/// Configuration for [`StateCache`] TTL tiers.
#[derive(Debug, Clone, Copy)]
pub struct StateCacheConfig {
    /// TTL for slowly-changing data: fees, bounds (seconds).
    pub slow_ttl: u64,
    /// TTL for fast-changing data: prices, funding rates, balances (seconds).
    pub fast_ttl: u64,
}

impl Default for StateCacheConfig {
    fn default() -> Self {
        Self {
            slow_ttl: 60,
            fast_ttl: 2,
        }
    }
}

/// Multi-layer TTL cache for on-chain state.
///
/// Keyed by address (`[u8; 20]`) for per-market data, or by perp ID
/// (`[u8; 32]`) for per-perp data. The USDC balance is a singleton.
#[derive(Debug)]
pub struct StateCache {
    // Slow layer (60s TTL): governance-controlled
    fees: HashMap<[u8; 20], CachedValue<CachedFees>>,
    bounds: HashMap<[u8; 20], CachedValue<CachedBounds>>,

    // Fast layer (2s TTL): changes every block
    mark_prices: HashMap<[u8; 32], CachedValue<f64>>,
    funding_rates: HashMap<[u8; 32], CachedValue<f64>>,
    usdc_balance: Option<CachedValue<f64>>,

    slow_ttl: u64,
    fast_ttl: u64,
}

impl StateCache {
    /// Create a new cache with the given TTL configuration.
    pub fn new(config: StateCacheConfig) -> Self {
        Self {
            fees: HashMap::new(),
            bounds: HashMap::new(),
            mark_prices: HashMap::new(),
            funding_rates: HashMap::new(),
            usdc_balance: None,
            slow_ttl: config.slow_ttl,
            fast_ttl: config.fast_ttl,
        }
    }

    // ── Slow layer: fees ───────────────────────────────────────────

    /// Get cached fees for a market address, or `None` if stale/absent.
    #[inline]
    pub fn get_fees(&self, addr: &[u8; 20], now_ts: u64) -> Option<&CachedFees> {
        self.fees
            .get(addr)
            .filter(|cv| cv.is_valid(now_ts))
            .map(|cv| &cv.value)
    }

    /// Cache fees for a market address.
    pub fn put_fees(&mut self, addr: [u8; 20], value: CachedFees, now_ts: u64) {
        self.fees.insert(
            addr,
            CachedValue {
                value,
                expires_at: now_ts.saturating_add(self.slow_ttl),
            },
        );
    }

    // ── Slow layer: bounds ─────────────────────────────────────────

    /// Get cached bounds for a market address, or `None` if stale/absent.
    #[inline]
    pub fn get_bounds(&self, addr: &[u8; 20], now_ts: u64) -> Option<&CachedBounds> {
        self.bounds
            .get(addr)
            .filter(|cv| cv.is_valid(now_ts))
            .map(|cv| &cv.value)
    }

    /// Cache bounds for a market address.
    pub fn put_bounds(&mut self, addr: [u8; 20], value: CachedBounds, now_ts: u64) {
        self.bounds.insert(
            addr,
            CachedValue {
                value,
                expires_at: now_ts.saturating_add(self.slow_ttl),
            },
        );
    }

    // ── Fast layer: mark prices ────────────────────────────────────

    /// Get cached mark price for a perp, or `None` if stale/absent.
    #[inline]
    pub fn get_mark_price(&self, perp_id: &[u8; 32], now_ts: u64) -> Option<f64> {
        self.mark_prices
            .get(perp_id)
            .filter(|cv| cv.is_valid(now_ts))
            .map(|cv| cv.value)
    }

    /// Cache a mark price for a perp.
    pub fn put_mark_price(&mut self, perp_id: [u8; 32], price: f64, now_ts: u64) {
        self.mark_prices.insert(
            perp_id,
            CachedValue {
                value: price,
                expires_at: now_ts.saturating_add(self.fast_ttl),
            },
        );
    }

    // ── Fast layer: funding rates ──────────────────────────────────

    /// Get cached funding rate for a perp, or `None` if stale/absent.
    #[inline]
    pub fn get_funding_rate(&self, perp_id: &[u8; 32], now_ts: u64) -> Option<f64> {
        self.funding_rates
            .get(perp_id)
            .filter(|cv| cv.is_valid(now_ts))
            .map(|cv| cv.value)
    }

    /// Cache a funding rate for a perp.
    pub fn put_funding_rate(&mut self, perp_id: [u8; 32], rate: f64, now_ts: u64) {
        self.funding_rates.insert(
            perp_id,
            CachedValue {
                value: rate,
                expires_at: now_ts.saturating_add(self.fast_ttl),
            },
        );
    }

    // ── Fast layer: USDC balance ───────────────────────────────────

    /// Get cached USDC balance, or `None` if stale/absent.
    #[inline]
    pub fn get_usdc_balance(&self, now_ts: u64) -> Option<f64> {
        self.usdc_balance
            .filter(|cv| cv.is_valid(now_ts))
            .map(|cv| cv.value)
    }

    /// Cache the USDC balance.
    pub fn put_usdc_balance(&mut self, balance: f64, now_ts: u64) {
        self.usdc_balance = Some(CachedValue {
            value: balance,
            expires_at: now_ts.saturating_add(self.fast_ttl),
        });
    }

    // ── Invalidation ───────────────────────────────────────────────

    /// Invalidate all fast-layer data (prices, funding, balance).
    ///
    /// Call on new-block events. The slow layer (fees, bounds) is preserved.
    pub fn invalidate_fast_layer(&mut self) {
        self.mark_prices.clear();
        self.funding_rates.clear();
        self.usdc_balance = None;
    }

    /// Invalidate everything (both layers).
    pub fn invalidate_all(&mut self) {
        self.fees.clear();
        self.bounds.clear();
        self.invalidate_fast_layer();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_fees() -> CachedFees {
        CachedFees {
            creator_fee: 0.001,
            insurance_fee: 0.0005,
            lp_fee: 0.003,
            liquidation_fee: 0.01,
        }
    }

    fn sample_bounds() -> CachedBounds {
        CachedBounds {
            min_margin: 5.0,
            min_taker_leverage: 1.0,
            max_taker_leverage: 100.0,
            liquidation_taker_ratio: 0.05,
        }
    }

    #[test]
    fn empty_cache_returns_none() {
        let c = StateCache::new(StateCacheConfig::default());
        assert!(c.get_fees(&[0; 20], 0).is_none());
        assert!(c.get_bounds(&[0; 20], 0).is_none());
        assert!(c.get_mark_price(&[0; 32], 0).is_none());
        assert!(c.get_funding_rate(&[0; 32], 0).is_none());
        assert!(c.get_usdc_balance(0).is_none());
    }

    #[test]
    fn slow_layer_respects_ttl() {
        let mut c = StateCache::new(StateCacheConfig::default()); // slow_ttl = 60
        let addr = [0xAA; 20];

        c.put_fees(addr, sample_fees(), 1000);
        // Valid at 1059 (59s elapsed < 60s TTL)
        assert!(c.get_fees(&addr, 1059).is_some());
        // Expired at 1060 (60s elapsed >= 60s TTL)
        assert!(c.get_fees(&addr, 1060).is_none());
    }

    #[test]
    fn fast_layer_respects_ttl() {
        let mut c = StateCache::new(StateCacheConfig::default()); // fast_ttl = 2
        let perp = [0xBB; 32];

        c.put_mark_price(perp, 42000.0, 1000);
        assert_eq!(c.get_mark_price(&perp, 1001), Some(42000.0));
        assert!(c.get_mark_price(&perp, 1002).is_none());
    }

    #[test]
    fn funding_rate_ttl() {
        let mut c = StateCache::new(StateCacheConfig::default());
        let perp = [0xCC; 32];

        c.put_funding_rate(perp, 0.0001, 500);
        assert_eq!(c.get_funding_rate(&perp, 501), Some(0.0001));
        assert!(c.get_funding_rate(&perp, 502).is_none());
    }

    #[test]
    fn usdc_balance_ttl() {
        let mut c = StateCache::new(StateCacheConfig::default());

        c.put_usdc_balance(10_000.0, 100);
        assert_eq!(c.get_usdc_balance(101), Some(10_000.0));
        assert!(c.get_usdc_balance(102).is_none());
    }

    #[test]
    fn bounds_caching() {
        let mut c = StateCache::new(StateCacheConfig::default());
        let addr = [0xDD; 20];

        c.put_bounds(addr, sample_bounds(), 0);
        let b = c.get_bounds(&addr, 30).unwrap();
        assert_eq!(b.max_taker_leverage, 100.0);
        assert_eq!(b.min_margin, 5.0);
    }

    #[test]
    fn invalidate_fast_preserves_slow() {
        let mut c = StateCache::new(StateCacheConfig::default());
        let addr = [0xAA; 20];
        let perp = [0xBB; 32];

        c.put_fees(addr, sample_fees(), 0);
        c.put_bounds(addr, sample_bounds(), 0);
        c.put_mark_price(perp, 42000.0, 0);
        c.put_funding_rate(perp, 0.0001, 0);
        c.put_usdc_balance(1000.0, 0);

        c.invalidate_fast_layer();

        // Slow layer survives
        assert!(c.get_fees(&addr, 0).is_some());
        assert!(c.get_bounds(&addr, 0).is_some());

        // Fast layer cleared
        assert!(c.get_mark_price(&perp, 0).is_none());
        assert!(c.get_funding_rate(&perp, 0).is_none());
        assert!(c.get_usdc_balance(0).is_none());
    }

    #[test]
    fn invalidate_all_clears_everything() {
        let mut c = StateCache::new(StateCacheConfig::default());
        let addr = [0xAA; 20];
        let perp = [0xBB; 32];

        c.put_fees(addr, sample_fees(), 0);
        c.put_mark_price(perp, 42000.0, 0);

        c.invalidate_all();

        assert!(c.get_fees(&addr, 0).is_none());
        assert!(c.get_mark_price(&perp, 0).is_none());
    }

    #[test]
    fn overwrite_updates_value_and_ttl() {
        let mut c = StateCache::new(StateCacheConfig::default());
        let perp = [0xBB; 32];

        c.put_mark_price(perp, 42000.0, 100);
        c.put_mark_price(perp, 43000.0, 200);

        // Old value gone (would have expired at 102), new value valid
        assert_eq!(c.get_mark_price(&perp, 201), Some(43000.0));
        assert!(c.get_mark_price(&perp, 202).is_none());
    }

    #[test]
    fn custom_config_ttls() {
        let config = StateCacheConfig {
            slow_ttl: 10,
            fast_ttl: 1,
        };
        let mut c = StateCache::new(config);
        let addr = [0xAA; 20];
        let perp = [0xBB; 32];

        c.put_fees(addr, sample_fees(), 0);
        c.put_mark_price(perp, 100.0, 0);

        // Custom slow TTL: 10s
        assert!(c.get_fees(&addr, 9).is_some());
        assert!(c.get_fees(&addr, 10).is_none());

        // Custom fast TTL: 1s
        assert!(c.get_mark_price(&perp, 0).is_some());
        assert!(c.get_mark_price(&perp, 1).is_none());
    }

    #[test]
    fn different_keys_independent() {
        let mut c = StateCache::new(StateCacheConfig::default());
        let perp_a = [0xAA; 32];
        let perp_b = [0xBB; 32];

        c.put_mark_price(perp_a, 100.0, 0);
        c.put_mark_price(perp_b, 200.0, 0);

        assert_eq!(c.get_mark_price(&perp_a, 0), Some(100.0));
        assert_eq!(c.get_mark_price(&perp_b, 0), Some(200.0));
    }

    #[test]
    fn cached_value_is_valid_boundary() {
        let cv = CachedValue {
            value: 42,
            expires_at: 100,
        };
        assert!(cv.is_valid(99)); // 1 second before
        assert!(!cv.is_valid(100)); // exactly at expiry
        assert!(!cv.is_valid(101)); // after expiry
    }
}
