//! Criterion benchmarks for hot-path math functions.
//!
//! These functions are called on every price update in an HFT loop.
//! We benchmark the realistic call patterns a trading system would use.

use criterion::{Criterion, black_box, criterion_group, criterion_main};

use alloy::primitives::I256;
use perpcity_rust_sdk::math::tick::{get_sqrt_ratio_at_tick, price_to_tick, tick_to_price};
use perpcity_rust_sdk::math::position::{entry_price, liquidation_price};

// ---------------------------------------------------------------------------
// Tick math benchmarks
// ---------------------------------------------------------------------------

fn bench_get_sqrt_ratio_at_tick(c: &mut Criterion) {
    let mut group = c.benchmark_group("tick_math");

    // Tick 0 — baseline, minimal bit manipulation
    group.bench_function("get_sqrt_ratio_at_tick/tick_0", |b| {
        b.iter(|| get_sqrt_ratio_at_tick(black_box(0)))
    });

    // Typical trading tick — several bits set in abs_tick
    group.bench_function("get_sqrt_ratio_at_tick/tick_1000", |b| {
        b.iter(|| get_sqrt_ratio_at_tick(black_box(1000)))
    });

    // Negative tick — tests the U256::MAX / ratio path
    group.bench_function("get_sqrt_ratio_at_tick/tick_neg1000", |b| {
        b.iter(|| get_sqrt_ratio_at_tick(black_box(-1000)))
    });

    // Worst case — all bits set, maximum number of U256 multiplications
    group.bench_function("get_sqrt_ratio_at_tick/tick_max", |b| {
        b.iter(|| get_sqrt_ratio_at_tick(black_box(887_272)))
    });

    // Protocol boundary tick
    group.bench_function("get_sqrt_ratio_at_tick/tick_69090", |b| {
        b.iter(|| get_sqrt_ratio_at_tick(black_box(69_090)))
    });

    group.finish();
}

fn bench_tick_to_price(c: &mut Criterion) {
    let mut group = c.benchmark_group("tick_to_price");

    group.bench_function("tick_0", |b| {
        b.iter(|| tick_to_price(black_box(0)))
    });

    group.bench_function("tick_1000", |b| {
        b.iter(|| tick_to_price(black_box(1000)))
    });

    group.bench_function("tick_neg1000", |b| {
        b.iter(|| tick_to_price(black_box(-1000)))
    });

    group.bench_function("tick_69090", |b| {
        b.iter(|| tick_to_price(black_box(69_090)))
    });

    group.finish();
}

fn bench_price_to_tick(c: &mut Criterion) {
    let mut group = c.benchmark_group("price_to_tick");

    group.bench_function("price_1.0", |b| {
        b.iter(|| price_to_tick(black_box(1.0)))
    });

    // Typical ETH/USD-ish price
    group.bench_function("price_1500.0", |b| {
        b.iter(|| price_to_tick(black_box(1500.0)))
    });

    // Small price — large negative tick
    group.bench_function("price_0.001", |b| {
        b.iter(|| price_to_tick(black_box(0.001)))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Position math benchmarks
// ---------------------------------------------------------------------------

fn bench_entry_price(c: &mut Criterion) {
    let mut group = c.benchmark_group("position_math");

    // Long 1 ETH at $1500
    let perp_delta = I256::try_from(1_000_000i64).unwrap();
    let usd_delta = I256::try_from(-1_500_000_000i64).unwrap();

    group.bench_function("entry_price", |b| {
        b.iter(|| entry_price(black_box(perp_delta), black_box(usd_delta)))
    });

    group.finish();
}

fn bench_liquidation_price(c: &mut Criterion) {
    let mut group = c.benchmark_group("position_math");

    let perp_delta = I256::try_from(1_000_000i64).unwrap();
    let usd_delta = I256::try_from(-1_500_000_000i64).unwrap();

    // Long liquidation
    group.bench_function("liquidation_price_long", |b| {
        b.iter(|| {
            liquidation_price(
                black_box(perp_delta),
                black_box(usd_delta),
                black_box(100.0),
                black_box(25_000),
                black_box(true),
            )
        })
    });

    // Short liquidation
    let perp_delta_short = I256::try_from(-1_000_000i64).unwrap();
    let usd_delta_short = I256::try_from(1_500_000_000i64).unwrap();

    group.bench_function("liquidation_price_short", |b| {
        b.iter(|| {
            liquidation_price(
                black_box(perp_delta_short),
                black_box(usd_delta_short),
                black_box(100.0),
                black_box(25_000),
                black_box(false),
            )
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Composite: simulate a price-update hot path
// ---------------------------------------------------------------------------

fn bench_price_update_hot_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("hot_path");

    // Simulate what happens on every price update:
    // 1. Convert new tick → price
    // 2. Compute liquidation price for a position
    let perp_delta = I256::try_from(1_000_000i64).unwrap();
    let usd_delta = I256::try_from(-1_500_000_000i64).unwrap();

    group.bench_function("tick_to_price_then_liquidation", |b| {
        b.iter(|| {
            let _price = tick_to_price(black_box(1000));
            liquidation_price(
                black_box(perp_delta),
                black_box(usd_delta),
                black_box(100.0),
                black_box(25_000),
                black_box(true),
            )
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_get_sqrt_ratio_at_tick,
    bench_tick_to_price,
    bench_price_to_tick,
    bench_entry_price,
    bench_liquidation_price,
    bench_price_update_hot_path,
);
criterion_main!(benches);
