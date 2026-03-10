//! Criterion benchmarks for transport layer hot paths.
//!
//! These benchmarks measure the critical-path operations that occur on every
//! RPC call: endpoint selection, health check/recording, and fan-out overhead.
//!
//! Priority order (matching PERFORMANCE PRIORITY ORDER):
//! 1. Endpoint selection latency (determines which endpoint gets the request)
//! 2. Health recording overhead (happens on every response)
//! 3. Hedged fan-out (task spawn overhead per hedged request)
//! 4. Struct sizes (cache-line analysis)

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use std::time::Duration;

use perpcity_rust_sdk::transport::config::{CircuitBreakerConfig, Strategy, TransportConfig};
use perpcity_rust_sdk::transport::health::{CircuitState, EndpointHealth};
use perpcity_rust_sdk::transport::provider::HftTransport;

// ---------------------------------------------------------------------------
// Helper: build a transport with N endpoints
// ---------------------------------------------------------------------------

fn make_transport(n: usize, strategy: Strategy) -> HftTransport {
    let mut builder = TransportConfig::builder().strategy(strategy);
    for i in 0..n {
        builder = builder.endpoint(format!("https://rpc{i}.example.com"));
    }
    HftTransport::new(builder.build().unwrap()).unwrap()
}

fn make_warm_transport(n: usize, strategy: Strategy) -> HftTransport {
    let t = make_transport(n, strategy);
    // Seed health data so latency-based selection has real values
    for i in 0..n {
        // Varying latencies: endpoint 0 = 5ms, 1 = 2ms, 2 = 8ms, etc.
        let latency_ns = match i % 4 {
            0 => 5_000_000,
            1 => 2_000_000,
            2 => 8_000_000,
            3 => 1_000_000,
            _ => unreachable!(),
        };
        t.record_success(i, latency_ns);
    }
    t
}

fn default_cb_config() -> CircuitBreakerConfig {
    CircuitBreakerConfig {
        failure_threshold: 3,
        recovery_timeout: Duration::from_secs(30),
        half_open_max_requests: 1,
    }
}

// ---------------------------------------------------------------------------
// EndpointHealth benchmarks (direct, no transport overhead)
// ---------------------------------------------------------------------------

fn bench_endpoint_health(c: &mut Criterion) {
    let mut group = c.benchmark_group("endpoint_health");

    // Hot path: is_callable on Closed endpoint — should be ~1-5ns
    group.bench_function("is_callable/closed", |b| {
        let mut h = EndpointHealth::new(default_cb_config());
        b.iter(|| h.is_callable(black_box(1000)))
    });

    // is_callable on Open endpoint (within recovery timeout) — should reject fast
    group.bench_function("is_callable/open_reject", |b| {
        let mut h = EndpointHealth::new(default_cb_config());
        for t in 1..=3 {
            h.record_failure(t * 1000);
        }
        b.iter(|| h.is_callable(black_box(5000))) // within 30s recovery
    });

    // is_callable on Open endpoint (past recovery) — transitions to HalfOpen
    // Note: this mutates state, so we rebuild each iteration
    group.bench_function("is_callable/open_to_halfopen", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let mut h = EndpointHealth::new(default_cb_config());
                for t in 1..=3 {
                    h.record_failure(t * 1000);
                }
                let start = std::time::Instant::now();
                let _ = h.is_callable(black_box(33_000));
                total += start.elapsed();
            }
            total
        })
    });

    // record_success with latency EMA update
    group.bench_function("record_success", |b| {
        let mut h = EndpointHealth::new(default_cb_config());
        h.record_success(5_000_000); // seed EMA
        b.iter(|| h.record_success(black_box(3_000_000)))
    });

    // record_failure
    group.bench_function("record_failure", |b| {
        let mut h = EndpointHealth::new(default_cb_config());
        b.iter(|| h.record_failure(black_box(1000)))
    });

    // avg_latency_ns read (just a field access)
    group.bench_function("avg_latency_ns", |b| {
        let mut h = EndpointHealth::new(default_cb_config());
        h.record_success(5_000_000);
        b.iter(|| h.avg_latency_ns())
    });

    // status snapshot (copies all fields)
    group.bench_function("status_snapshot", |b| {
        let mut h = EndpointHealth::new(default_cb_config());
        h.record_success(5_000_000);
        b.iter(|| h.status())
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Endpoint selection benchmarks (through HftTransport)
// ---------------------------------------------------------------------------

fn bench_endpoint_selection(c: &mut Criterion) {
    let mut group = c.benchmark_group("endpoint_selection");

    // Round-robin with 3 endpoints — atomic fetch_add + mutex check
    group.bench_function("round_robin/3ep", |b| {
        let t = make_warm_transport(3, Strategy::RoundRobin);
        b.iter(|| t.select_endpoint(black_box(1000)))
    });

    // Latency-based with 3 endpoints — lock all, find min
    group.bench_function("latency_based/3ep", |b| {
        let t = make_warm_transport(3, Strategy::LatencyBased);
        b.iter(|| t.select_endpoint(black_box(1000)))
    });

    // Latency-based with 5 endpoints
    group.bench_function("latency_based/5ep", |b| {
        let t = make_warm_transport(5, Strategy::LatencyBased);
        b.iter(|| t.select_endpoint(black_box(1000)))
    });

    // Latency-based with 10 endpoints (stress test)
    group.bench_function("latency_based/10ep", |b| {
        let t = make_warm_transport(10, Strategy::LatencyBased);
        b.iter(|| t.select_endpoint(black_box(1000)))
    });

    // select_n_endpoints for hedged requests (3-way fan-out from 5 endpoints)
    group.bench_function("select_n/fan3_from5", |b| {
        let t = make_warm_transport(5, Strategy::LatencyBased);
        b.iter(|| t.select_n_endpoints(black_box(3), black_box(1000)))
    });

    // select_n_endpoints — 2-way fan-out from 3 endpoints
    group.bench_function("select_n/fan2_from3", |b| {
        let t = make_warm_transport(3, Strategy::LatencyBased);
        b.iter(|| t.select_n_endpoints(black_box(2), black_box(1000)))
    });

    // Worst case: all endpoints have open circuits except one
    group.bench_function("latency_based/1_of_3_healthy", |b| {
        let t = make_warm_transport(3, Strategy::LatencyBased);
        // Trip circuits on endpoints 0 and 1
        for t_ms in 1..=3 {
            t.record_failure(0, t_ms * 1000);
            t.record_failure(1, t_ms * 1000);
        }
        b.iter(|| t.select_endpoint(black_box(5000)))
    });

    // All endpoints open — should return None fast
    group.bench_function("latency_based/all_open", |b| {
        let t = make_warm_transport(3, Strategy::LatencyBased);
        for ep in 0..3 {
            for t_ms in 1..=3 {
                t.record_failure(ep, t_ms * 1000);
            }
        }
        b.iter(|| t.select_endpoint(black_box(5000)))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Health recording through transport (includes Mutex overhead)
// ---------------------------------------------------------------------------

fn bench_health_recording(c: &mut Criterion) {
    let mut group = c.benchmark_group("health_recording");

    // record_success through transport (Mutex lock + EMA update)
    group.bench_function("record_success/through_transport", |b| {
        let t = make_warm_transport(3, Strategy::LatencyBased);
        b.iter(|| t.record_success(black_box(1), black_box(3_000_000)))
    });

    // record_failure through transport
    group.bench_function("record_failure/through_transport", |b| {
        let t = make_warm_transport(3, Strategy::LatencyBased);
        b.iter(|| t.record_failure(black_box(1), black_box(1000)))
    });

    // health_status — locks all endpoints, copies status
    group.bench_function("health_status/3ep", |b| {
        let t = make_warm_transport(3, Strategy::LatencyBased);
        b.iter(|| t.health_status())
    });

    group.bench_function("health_status/5ep", |b| {
        let t = make_warm_transport(5, Strategy::LatencyBased);
        b.iter(|| t.health_status())
    });

    // healthy_count — locks all, filters
    group.bench_function("healthy_count/3ep", |b| {
        let t = make_warm_transport(3, Strategy::LatencyBased);
        b.iter(|| t.healthy_count())
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// now_ms() syscall overhead
// ---------------------------------------------------------------------------

fn bench_time(c: &mut Criterion) {
    let mut group = c.benchmark_group("time");

    // SystemTime::now() + conversion — this is called on every request
    group.bench_function("now_ms_syscall", |b| {
        b.iter(|| {
            black_box(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            )
        })
    });

    // Instant::now() for comparison (monotonic, lighter on some platforms)
    group.bench_function("instant_now", |b| {
        b.iter(|| black_box(std::time::Instant::now()))
    });

    // Instant elapsed (used in latency tracking)
    group.bench_function("instant_elapsed", |b| {
        let start = std::time::Instant::now();
        b.iter(|| black_box(start.elapsed().as_nanos() as u64))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Struct sizes — verify cache-line impact
// ---------------------------------------------------------------------------

fn bench_struct_sizes(c: &mut Criterion) {
    use perpcity_rust_sdk::transport::health::EndpointStatus;

    let mut group = c.benchmark_group("transport_struct_sizes");

    group.bench_function("verify_sizes", |b| {
        b.iter(|| {
            let health_size = std::mem::size_of::<EndpointHealth>();
            let status_size = std::mem::size_of::<EndpointStatus>();
            let state_size = std::mem::size_of::<CircuitState>();

            // EndpointHealth should fit in 1-2 cache lines for fast Mutex access
            assert!(
                health_size <= 128,
                "EndpointHealth exceeds 2 cache lines: {health_size}"
            );
            // EndpointStatus should be cheap to copy
            assert!(
                status_size <= 64,
                "EndpointStatus exceeds cache line: {status_size}"
            );
            // CircuitState should be tiny
            assert!(state_size <= 16, "CircuitState too large: {state_size}");

            black_box((health_size, status_size, state_size))
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Composite: simulate endpoint selection + health recording cycle
// ---------------------------------------------------------------------------

fn bench_request_cycle(c: &mut Criterion) {
    let mut group = c.benchmark_group("request_cycle");

    // Full cycle: select_endpoint + record_success (simulates one RPC round-trip)
    group.bench_function("select_and_record/3ep", |b| {
        let t = make_warm_transport(3, Strategy::LatencyBased);
        b.iter(|| {
            let idx = t.select_endpoint(black_box(1000)).unwrap();
            t.record_success(black_box(idx), black_box(5_000_000));
        })
    });

    // Hedged: select_n + record for winner
    group.bench_function("hedged_select_and_record/fan3_from5", |b| {
        let t = make_warm_transport(5, Strategy::LatencyBased);
        b.iter(|| {
            let indices = t.select_n_endpoints(black_box(3), black_box(1000));
            // Simulate: first endpoint (best latency) wins
            if let Some(&idx) = indices.first() {
                t.record_success(black_box(idx), black_box(2_000_000));
            }
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_endpoint_health,
    bench_endpoint_selection,
    bench_health_recording,
    bench_time,
    bench_struct_sizes,
    bench_request_cycle,
);
criterion_main!(benches);
