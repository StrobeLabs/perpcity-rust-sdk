#![allow(clippy::assertions_on_constants)]
//! Criterion benchmarks for HFT infrastructure hot paths.
//!
//! These benchmarks measure the critical-path operations a trading bot
//! executes on every order: nonce acquisition, gas fee lookup, state cache
//! reads, latency recording, and the full pipeline prepare() call.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use std::collections::HashMap;

use perpcity_rust_sdk::hft::gas::{GasCache, GasLimits, Urgency};
use perpcity_rust_sdk::hft::latency::LatencyTracker;
use perpcity_rust_sdk::hft::nonce::NonceManager;
use perpcity_rust_sdk::hft::pipeline::{PipelineConfig, TxPipeline, TxRequest};
use perpcity_rust_sdk::hft::position_manager::{ManagedPosition, PositionManager};
use perpcity_rust_sdk::hft::state_cache::{CachedFees, StateCache, StateCacheConfig};

// ---------------------------------------------------------------------------
// Nonce benchmarks
// ---------------------------------------------------------------------------

fn bench_nonce(c: &mut Criterion) {
    let mut group = c.benchmark_group("nonce");

    // Hot path: single atomic fetch_add — should be ~1-5ns
    group.bench_function("acquire", |b| {
        let mgr = NonceManager::new(0);
        b.iter(|| mgr.acquire())
    });

    // Peek: single atomic load — should be ~1ns
    group.bench_function("peek", |b| {
        let mgr = NonceManager::new(0);
        b.iter(|| mgr.peek())
    });

    // Acquire + release cycle (CAS rewind on last-acquired nonce)
    group.bench_function("acquire_release_cycle", |b| {
        let mgr = NonceManager::new(0);
        b.iter(|| {
            let n = mgr.acquire();
            mgr.release(n);
        })
    });

    // Track (cold path, takes mutex + HashMap insert)
    group.bench_function("track", |b| {
        let mgr = NonceManager::new(0);
        let mut nonce = 0u64;
        b.iter(|| {
            mgr.track(black_box(nonce), black_box([0xAA; 32]), black_box(1000));
            nonce += 1;
        })
    });

    // Confirm (cold path, mutex + HashMap remove)
    group.bench_function("confirm", |b| {
        let mgr = NonceManager::new(0);
        // Pre-populate with entries to remove
        for i in 0..10_000u64 {
            mgr.track(i, [0xBB; 32], 1000);
        }
        let mut nonce = 0u64;
        b.iter(|| {
            let n = nonce;
            nonce += 1;
            mgr.confirm(black_box(n))
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Gas cache benchmarks
// ---------------------------------------------------------------------------

fn bench_gas(c: &mut Criterion) {
    let mut group = c.benchmark_group("gas");

    // Hot path: fees_for() — O(1) saturating arithmetic
    group.bench_function("fees_for/normal", |b| {
        let mut cache = GasCache::new(2000, 1_000_000_000);
        cache.update(50_000_000, 1000);
        b.iter(|| cache.fees_for(black_box(Urgency::Normal), black_box(1500)))
    });

    group.bench_function("fees_for/critical", |b| {
        let mut cache = GasCache::new(2000, 1_000_000_000);
        cache.update(50_000_000, 1000);
        b.iter(|| cache.fees_for(black_box(Urgency::Critical), black_box(1500)))
    });

    // is_valid check — branch-on-option + subtraction + comparison
    group.bench_function("is_valid", |b| {
        let mut cache = GasCache::new(2000, 1_000_000_000);
        cache.update(50_000_000, 1000);
        b.iter(|| cache.is_valid(black_box(1500)))
    });

    // Stale cache — should return None fast
    group.bench_function("fees_for/stale", |b| {
        let mut cache = GasCache::new(2000, 1_000_000_000);
        cache.update(50_000_000, 0);
        b.iter(|| cache.fees_for(black_box(Urgency::Normal), black_box(5000)))
    });

    // Cold path: update
    group.bench_function("update", |b| {
        let mut cache = GasCache::new(2000, 1_000_000_000);
        b.iter(|| cache.update(black_box(50_000_000), black_box(1000)))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// State cache benchmarks
// ---------------------------------------------------------------------------

fn bench_state_cache(c: &mut Criterion) {
    let mut group = c.benchmark_group("state_cache");

    // Hot path: mark price read (HashMap lookup + TTL check)
    group.bench_function("get_mark_price/hit", |b| {
        let mut cache = StateCache::new(StateCacheConfig::default());
        let perp = [0xAA; 32];
        cache.put_mark_price(perp, 42000.0, 1000);
        b.iter(|| cache.get_mark_price(black_box(&perp), black_box(1001)))
    });

    // Mark price read — miss (key not present)
    group.bench_function("get_mark_price/miss", |b| {
        let cache = StateCache::new(StateCacheConfig::default());
        let perp = [0xBB; 32];
        b.iter(|| cache.get_mark_price(black_box(&perp), black_box(1001)))
    });

    // Mark price read — expired
    group.bench_function("get_mark_price/expired", |b| {
        let mut cache = StateCache::new(StateCacheConfig::default());
        let perp = [0xAA; 32];
        cache.put_mark_price(perp, 42000.0, 1000);
        b.iter(|| cache.get_mark_price(black_box(&perp), black_box(1003))) // fast_ttl = 2s
    });

    // Funding rate read
    group.bench_function("get_funding_rate/hit", |b| {
        let mut cache = StateCache::new(StateCacheConfig::default());
        let perp = [0xCC; 32];
        cache.put_funding_rate(perp, 0.0001, 1000);
        b.iter(|| cache.get_funding_rate(black_box(&perp), black_box(1001)))
    });

    // Fees read (slow layer, 20-byte key)
    group.bench_function("get_fees/hit", |b| {
        let mut cache = StateCache::new(StateCacheConfig::default());
        let addr = [0xDD; 20];
        cache.put_fees(
            addr,
            CachedFees {
                creator_fee: 0.001,
                insurance_fee: 0.0005,
                lp_fee: 0.003,
                liquidation_fee: 0.01,
            },
            1000,
        );
        b.iter(|| cache.get_fees(black_box(&addr), black_box(1050)))
    });

    // USDC balance read (singleton, no HashMap)
    group.bench_function("get_usdc_balance/hit", |b| {
        let mut cache = StateCache::new(StateCacheConfig::default());
        cache.put_usdc_balance(10_000.0, 1000);
        b.iter(|| cache.get_usdc_balance(black_box(1001)))
    });

    // Cold path: put_mark_price
    group.bench_function("put_mark_price", |b| {
        let mut cache = StateCache::new(StateCacheConfig::default());
        let perp = [0xAA; 32];
        b.iter(|| cache.put_mark_price(black_box(perp), black_box(42000.0), black_box(1000)))
    });

    // State cache with many entries (realistic warm cache)
    group.bench_function("get_mark_price/warm_cache_10", |b| {
        let mut cache = StateCache::new(StateCacheConfig::default());
        // Populate 10 perps
        for i in 0u8..10 {
            let mut perp = [0u8; 32];
            perp[0] = i;
            cache.put_mark_price(perp, 42000.0 + i as f64, 1000);
        }
        let target = {
            let mut p = [0u8; 32];
            p[0] = 5;
            p
        };
        b.iter(|| cache.get_mark_price(black_box(&target), black_box(1001)))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Latency tracker benchmarks
// ---------------------------------------------------------------------------

fn bench_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("latency");

    // Hot path: record() — O(1) circular buffer write
    group.bench_function("record", |b| {
        let mut tracker = LatencyTracker::new();
        let mut ns = 100_000u64;
        b.iter(|| {
            tracker.record(black_box(ns));
            ns = ns.wrapping_add(1);
        })
    });

    // record_elapsed — saturating_sub + record
    group.bench_function("record_elapsed", |b| {
        let mut tracker = LatencyTracker::new();
        b.iter(|| tracker.record_elapsed(black_box(1_000_000), black_box(2_500_000)))
    });

    // record() after filling the buffer (steady-state)
    group.bench_function("record/steady_state", |b| {
        let mut tracker = LatencyTracker::new();
        // Fill buffer first
        for i in 0..2048u64 {
            tracker.record(i * 1000);
        }
        let mut ns = 3_000_000u64;
        b.iter(|| {
            tracker.record(black_box(ns));
            ns = ns.wrapping_add(1);
        })
    });

    // Cold path: stats() — O(n log n) sort
    group.bench_function("stats/full_window", |b| {
        let mut tracker = LatencyTracker::new();
        for i in 0..1024u64 {
            tracker.record(i * 1000);
        }
        b.iter(|| tracker.stats())
    });

    // Stats with small window
    group.bench_function("stats/10_samples", |b| {
        let mut tracker = LatencyTracker::new();
        for i in 0..10u64 {
            tracker.record(i * 1000);
        }
        b.iter(|| tracker.stats())
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Pipeline benchmarks
// ---------------------------------------------------------------------------

fn bench_pipeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("pipeline");

    // Hot path: prepare() — zero RPC
    // This is the most critical benchmark: nonce acquire + gas lookup + in-flight check
    group.bench_function("prepare", |b| {
        let pipe = TxPipeline::new(0, PipelineConfig::default());
        let mut gc = GasCache::new(5000, 1_000_000_000);
        gc.update(50_000_000, 0);
        b.iter(|| {
            pipe.prepare(
                black_box(TxRequest {
                    to: [0xAA; 20],
                    calldata: vec![0x01, 0x02, 0x03, 0x04],
                    value: 0,
                    gas_limit: GasLimits::OPEN_TAKER,
                    urgency: Urgency::Normal,
                }),
                black_box(&gc),
                black_box(1000),
            )
        })
    });

    // prepare() with pre-allocated calldata (simulates zero-alloc hot path)
    group.bench_function("prepare/prealloc_calldata", |b| {
        let pipe = TxPipeline::new(0, PipelineConfig::default());
        let mut gc = GasCache::new(5000, 1_000_000_000);
        gc.update(50_000_000, 0);
        let calldata = vec![0x01, 0x02, 0x03, 0x04];
        b.iter(|| {
            pipe.prepare(
                TxRequest {
                    to: black_box([0xAA; 20]),
                    calldata: calldata.clone(),
                    value: 0,
                    gas_limit: GasLimits::OPEN_TAKER,
                    urgency: Urgency::Normal,
                },
                &gc,
                black_box(1000),
            )
        })
    });

    // prepare() fail-fast on stale gas
    group.bench_function("prepare/stale_gas_reject", |b| {
        let pipe = TxPipeline::new(0, PipelineConfig::default());
        let mut gc = GasCache::new(2000, 1_000_000_000);
        gc.update(50_000_000, 0);
        b.iter(|| {
            let _ = pipe.prepare(
                TxRequest {
                    to: [0xAA; 20],
                    calldata: vec![],
                    value: 0,
                    gas_limit: GasLimits::OPEN_TAKER,
                    urgency: Urgency::Normal,
                },
                &gc,
                black_box(5000), // stale
            );
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Position manager benchmarks
// ---------------------------------------------------------------------------

fn bench_position_manager(c: &mut Criterion) {
    let mut group = c.benchmark_group("position_manager");

    // check_triggers with a single position (stop-loss + take-profit)
    group.bench_function("check_triggers/1_pos", |b| {
        let mut mgr = PositionManager::new();
        mgr.track(ManagedPosition {
            perp_id: [0xAA; 32],
            position_id: 1,
            is_long: true,
            entry_price: 100.0,
            margin: 10.0,
            stop_loss: Some(90.0),
            take_profit: Some(120.0),
            trailing_stop_pct: Some(0.05),
            trailing_stop_anchor: Some(105.0),
        });
        let prices = HashMap::from([([0xAA; 32], 105.0)]);
        b.iter(|| mgr.check_triggers(black_box(&prices)))
    });

    // check_triggers with 10 positions (realistic trading bot)
    group.bench_function("check_triggers/10_pos", |b| {
        let mut mgr = PositionManager::new();
        for i in 0..10u64 {
            mgr.track(ManagedPosition {
                perp_id: [0xAA; 32],
                position_id: i,
                is_long: i % 2 == 0,
                entry_price: 100.0 + i as f64,
                margin: 10.0,
                stop_loss: Some(90.0),
                take_profit: Some(120.0),
                trailing_stop_pct: Some(0.05),
                trailing_stop_anchor: Some(105.0),
            });
        }
        let prices = HashMap::from([([0xAA; 32], 105.0)]);
        b.iter(|| mgr.check_triggers(black_box(&prices)))
    });

    // check_triggers_into with pre-allocated buffer (zero-alloc)
    group.bench_function("check_triggers_into/10_pos", |b| {
        let mut mgr = PositionManager::new();
        for i in 0..10u64 {
            mgr.track(ManagedPosition {
                perp_id: [0xAA; 32],
                position_id: i,
                is_long: i % 2 == 0,
                entry_price: 100.0 + i as f64,
                margin: 10.0,
                stop_loss: Some(90.0),
                take_profit: Some(120.0),
                trailing_stop_pct: Some(0.05),
                trailing_stop_anchor: Some(105.0),
            });
        }
        let prices = HashMap::from([([0xAA; 32], 105.0)]);
        let mut buf = Vec::with_capacity(16);
        b.iter(|| {
            buf.clear();
            mgr.check_triggers_into(black_box(&prices), &mut buf);
            black_box(&buf);
        })
    });

    // check_triggers — no triggers fire (common case in normal market)
    group.bench_function("check_triggers/no_fire", |b| {
        let mut mgr = PositionManager::new();
        mgr.track(ManagedPosition {
            perp_id: [0xAA; 32],
            position_id: 1,
            is_long: true,
            entry_price: 100.0,
            margin: 10.0,
            stop_loss: Some(80.0),
            take_profit: Some(150.0),
            trailing_stop_pct: None,
            trailing_stop_anchor: None,
        });
        let prices = HashMap::from([([0xAA; 32], 105.0)]);
        b.iter(|| mgr.check_triggers(black_box(&prices)))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Composite: simulate a full trading tick
// ---------------------------------------------------------------------------

fn bench_trading_tick(c: &mut Criterion) {
    let mut group = c.benchmark_group("trading_tick");

    // Full hot-path simulation: state cache read + pipeline prepare + latency record
    group.bench_function("cache_read_prepare_record", |b| {
        let pipe = TxPipeline::new(0, PipelineConfig::default());
        let mut gc = GasCache::new(5000, 1_000_000_000);
        gc.update(50_000_000, 0);
        let mut state = StateCache::new(StateCacheConfig::default());
        let perp = [0xAA; 32];
        state.put_mark_price(perp, 42000.0, 0);
        let mut lat = LatencyTracker::new();
        let calldata = vec![0x01, 0x02, 0x03, 0x04];

        b.iter(|| {
            // 1. Read mark price from cache
            let _price = state.get_mark_price(black_box(&perp), black_box(1));
            // 2. Prepare transaction (nonce + gas)
            let _prepared = pipe.prepare(
                TxRequest {
                    to: [0xAA; 20],
                    calldata: calldata.clone(),
                    value: 0,
                    gas_limit: GasLimits::OPEN_TAKER,
                    urgency: Urgency::Normal,
                },
                &gc,
                black_box(1000),
            );
            // 3. Record latency
            lat.record(black_box(500_000));
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Struct size verification benchmarks (compile-time sanity)
// ---------------------------------------------------------------------------

fn bench_struct_sizes(c: &mut Criterion) {
    use perpcity_rust_sdk::hft::gas::GasFees;
    use perpcity_rust_sdk::hft::latency::LatencyStats;
    use perpcity_rust_sdk::hft::nonce::PendingTx;

    let mut group = c.benchmark_group("struct_sizes");

    // These aren't really benchmarks — they verify struct sizes at bench time
    // and document the cache-line impact of each type.
    group.bench_function("verify_sizes", |b| {
        b.iter(|| {
            let pending_tx_size = std::mem::size_of::<PendingTx>();
            let gas_fees_size = std::mem::size_of::<GasFees>();
            let latency_stats_size = std::mem::size_of::<LatencyStats>();
            let cached_fees_size = std::mem::size_of::<CachedFees>();
            let managed_position_size = std::mem::size_of::<ManagedPosition>();

            // Assert reasonable sizes (catches accidental field additions)
            assert!(
                pending_tx_size <= 64,
                "PendingTx exceeds cache line: {pending_tx_size}"
            );
            assert!(
                gas_fees_size <= 64,
                "GasFees exceeds cache line: {gas_fees_size}"
            );
            assert!(
                latency_stats_size <= 64,
                "LatencyStats exceeds cache line: {latency_stats_size}"
            );
            assert!(
                cached_fees_size <= 64,
                "CachedFees exceeds cache line: {cached_fees_size}"
            );

            black_box((
                pending_tx_size,
                gas_fees_size,
                latency_stats_size,
                cached_fees_size,
                managed_position_size,
            ))
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_nonce,
    bench_gas,
    bench_state_cache,
    bench_latency,
    bench_pipeline,
    bench_position_manager,
    bench_trading_tick,
    bench_struct_sizes,
);
criterion_main!(benches);
