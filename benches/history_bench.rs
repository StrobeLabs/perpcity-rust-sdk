//! Where a replay's time goes: the scan, the decode and the fold, measured
//! apart, so a claim about any of them is a number.
//!
//! The tape is synthetic and in memory, served by the history readers' own
//! fake node, so the scan here measures the scanning pipeline and not a
//! provider's latency: windows, decoding into rows, the header reads a
//! provider that omits timestamps would cost. Set `PERPCITY_RECORDING` to
//! a recording's directory (the `record` example writes one) to decode and
//! fold a recorded market instead of the synthetic one.
//!
//! ```bash
//! cargo bench --features test-utils --bench history_bench
//! ```

use std::env;
use std::path::Path;
use std::thread;

use alloy::primitives::{Address, B256, Log as PrimitiveLog, U256};
use alloy::rpc::types::Log;
use alloy::sol_types::SolEvent;
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use perpcity_sdk::constants::Q96;
use perpcity_sdk::contracts::{Capacity, IBeacon, IPoolManagerState, OpenInterest, Perp};
use perpcity_sdk::events::{MarketEvent, decode_log};
use perpcity_sdk::history::test_support::FakeNode;
use perpcity_sdk::history::{History, Recording, TapeAddresses, TapeEvent};

const PERP: Address = Address::repeat_byte(0xF0);
const BEACON: Address = Address::repeat_byte(0xBE);
const POOL_MANAGER: Address = Address::repeat_byte(0x9A);
const POOL_ID: B256 = B256::repeat_byte(0x77);

/// Events per block in the synthetic tape, and the tape's length in events.
const PER_BLOCK: u64 = 4;
const EVENTS: u64 = 200_000;

/// A mined log carrying `event`, as a node that includes block timestamps
/// returns it.
fn mined<E: SolEvent>(event: &E, address: Address, block: u64, index: u64) -> Log {
    Log {
        inner: PrimitiveLog {
            address,
            data: event.encode_log_data(),
        },
        block_hash: Some(B256::with_last_byte(1)),
        block_number: Some(block),
        block_timestamp: Some(1_700_000_000 + block / 4),
        transaction_hash: Some(B256::with_last_byte(3)),
        transaction_index: Some(0),
        log_index: Some(index),
        removed: false,
    }
}

/// A market's life in miniature, `EVENTS` logs across three addresses: a
/// maker open and its liquidity, a print, and the market's totals, over and
/// over, with the mix a real tape has.
fn synthetic_logs() -> Vec<Log> {
    let mut logs = Vec::with_capacity(EVENTS as usize);
    for n in 0..EVENTS {
        let block = n / PER_BLOCK + 1;
        let index = n % PER_BLOCK;
        let log = match n % 4 {
            0 => mined(
                &Perp::MakerOpened {
                    posId: U256::from(n),
                },
                PERP,
                block,
                index,
            ),
            1 => mined(
                &IPoolManagerState::ModifyLiquidity {
                    id: POOL_ID,
                    sender: PERP,
                    tickLower: alloy::primitives::Signed::<24, 1>::try_from(-600).unwrap(),
                    tickUpper: alloy::primitives::Signed::<24, 1>::try_from(600).unwrap(),
                    liquidityDelta: alloy::primitives::I256::try_from(1_000_000_000i64).unwrap(),
                    salt: B256::from(U256::from(n)),
                },
                POOL_MANAGER,
                block,
                index,
            ),
            2 => mined(
                &IBeacon::IndexUpdated {
                    index: Q96 * U256::from(40 + n % 7),
                },
                BEACON,
                block,
                index,
            ),
            _ => mined(
                &Perp::OpenInterestUpdated {
                    oi: OpenInterest {
                        long: 1_000_000 + n as u128,
                        short: 900_000,
                    },
                },
                PERP,
                block,
                index,
            ),
        };
        logs.push(log);
    }
    // One capacity row so the fold's match has every family it reads.
    logs.push(mined(
        &Perp::CapacityUpdated {
            cap: Capacity {
                long: 5_000_000,
                short: 5_000_000,
            },
        },
        PERP,
        EVENTS / PER_BLOCK + 2,
        0,
    ));
    logs
}

fn addresses() -> TapeAddresses {
    TapeAddresses {
        perp: PERP,
        beacon: BEACON,
        pool_manager: POOL_MANAGER,
        pool_id: POOL_ID,
    }
}

/// The synthetic tape's last block: the scan's range is the market's life,
/// as a caller's would be, not the chain's.
const LAST_BLOCK: u64 = EVENTS / PER_BLOCK + 2;

/// A recorded tape when `PERPCITY_RECORDING` names a recording, decoded
/// from its raw logs, else the synthetic one scanned once through the fake
/// node.
fn tape(runtime: &tokio::runtime::Runtime, logs: &[Log]) -> Vec<TapeEvent> {
    if let Ok(dir) = env::var("PERPCITY_RECORDING") {
        return Recording::read(Path::new(&dir))
            .expect("PERPCITY_RECORDING is a recording directory")
            .tape()
            .expect("the recording decodes");
    }
    scan(runtime, logs)
}

/// One scan of `logs` through a fresh fake node: the pipeline's cost with
/// no network under it.
fn scan(runtime: &tokio::runtime::Runtime, logs: &[Log]) -> Vec<TapeEvent> {
    let node = FakeNode::new(logs.to_vec(), u64::MAX);
    let history = History::new(node.provider());
    runtime
        .block_on(history.market_tape(addresses(), 0, Some(LAST_BLOCK)))
        .expect("the synthetic tape scans")
}

/// Decode every log, in order.
fn decode_serial(logs: &[Log]) -> usize {
    logs.iter()
        .filter_map(|log| decode_log(log).ok().flatten())
        .count()
}

/// Decode every log, one thread per core over contiguous chunks; the
/// decoder is pure, so order is restored by concatenation.
fn decode_parallel(logs: &[Log]) -> usize {
    let threads = thread::available_parallelism().map_or(1, |n| n.get());
    let chunk = logs.len().div_ceil(threads).max(1);
    thread::scope(|scope| {
        let handles: Vec<_> = logs
            .chunks(chunk)
            .map(|part| scope.spawn(move || decode_serial(part)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).sum()
    })
}

/// The cheapest fold that touches every row: one `match` per event and a
/// running total, which is the shape every real fold has before its state.
fn fold_noop(tape: &[TapeEvent]) -> u64 {
    tape.iter().fold(0u64, |acc, row| {
        acc + match row.event {
            MarketEvent::MakerOpened { .. } => 1,
            MarketEvent::ModifyLiquidity { .. } => 2,
            MarketEvent::IndexUpdated { .. } => 3,
            MarketEvent::OpenInterestUpdated { .. } => 4,
            MarketEvent::CapacityUpdated { .. } => 5,
            _ => 7,
        } + (row.block_number & 1)
    })
}

fn bench_history(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let logs = synthetic_logs();
    let tape = tape(&runtime, &logs);
    let events = logs.len() as u64;

    let mut group = c.benchmark_group("history");
    group.sample_size(10);

    group.throughput(Throughput::Elements(events));
    group.bench_function(
        BenchmarkId::new("scan", "fake node, three addresses"),
        |b| b.iter(|| black_box(scan(&runtime, &logs).len())),
    );

    group.throughput(Throughput::Elements(events));
    group.bench_function(BenchmarkId::new("decode", "serial"), |b| {
        b.iter(|| black_box(decode_serial(&logs)))
    });
    group.bench_function(BenchmarkId::new("decode", "parallel"), |b| {
        b.iter(|| black_box(decode_parallel(&logs)))
    });

    group.throughput(Throughput::Elements(tape.len() as u64));
    group.bench_function(BenchmarkId::new("fold", "no-op match"), |b| {
        b.iter(|| black_box(fold_noop(&tape)))
    });

    group.finish();
}

criterion_group!(benches, bench_history);
criterion_main!(benches);
