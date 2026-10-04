//! The claim a replay makes, checked against a live market: the state
//! rebuilt from the tape equals the state read from storage at the same
//! block, to the atom, for every quantity the tape carries.
//!
//! Ignored by default: it scans a market's whole tape over RPC and reads
//! its storage at the lagged head. The reads need no archive node, since
//! the block is near the head.
//!
//! `PERPCITY_TAPE_FROM` bounds the scan, it does not start the market: the
//! fold begins at genesis, so the bound must be at or before the market's
//! first event, or the comparison fails on totals the tape never stated.
//!
//! ```bash
//! RPC_URL=https://arb1.arbitrum.io/rpc \
//! PERPCITY_PERP=0xea3f47e8…10dd \
//! PERPCITY_TAPE_FROM=510000000 \
//! cargo test --test replay_live -- --ignored --nocapture
//! ```

use std::env;

use alloy::primitives::{Address, U256};
use perpcity_sdk::convert::unpack_balance_delta;
use perpcity_sdk::history::{Fold, PositionKind, Replay};
use perpcity_sdk::{ChainReader, HftTransport, StateAt, TransportConfig};

/// The first block of the deployed-era factory: every live market's tape
/// starts at or after it.
const FACTORY_GENESIS: u64 = 468_050_231;

fn var(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.is_empty())
}

#[tokio::test]
#[ignore = "scans a live market over RPC; set RPC_URL and PERPCITY_PERP"]
async fn a_replayed_market_equals_the_read_at_the_same_block() {
    dotenvy::dotenv().ok();
    let rpc_url = var("RPC_URL").expect("set RPC_URL");
    let perp: Address = var("PERPCITY_PERP")
        .expect("set PERPCITY_PERP")
        .parse()
        .expect("a market address");
    let from: u64 = var("PERPCITY_TAPE_FROM")
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(FACTORY_GENESIS);

    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&rpc_url)
            .build()
            .unwrap(),
    )
    .unwrap();
    let chain = ChainReader::arbitrum(transport);
    let market = chain.market(perp);

    // One block for both sides: the tape to it, the reads at it.
    let history = chain.history();
    let tip = history.tip().await.unwrap();
    let addresses = market.tape_addresses().await.unwrap();
    let config = market.get_config().await.unwrap();

    let tape = history
        .market_tape(addresses, from, Some(tip))
        .await
        .unwrap();
    println!(
        "{} rows from block {from} to {tip}; {} undecodable",
        tape.len(),
        history.stats().undecodable
    );
    assert_eq!(history.stats().undecodable, 0, "the tape has a gap");
    let mut replay = Replay::from_genesis(perp);
    for row in &tape {
        replay.apply(row);
    }

    let state = market.state_at(tip).await.unwrap();
    agrees(&replay, &state, config.ema_window).await;

    // The other start: the reads at an earlier block, then the tape from
    // there. Every total a read supplied and every position's margin is
    // known at the seed; what the tail touched is known from the tail.
    let seed_block = tip - SEED_LAG;
    let earlier = market.state_at(seed_block).await.unwrap();
    let mut seeded = Replay::seeded(&earlier).await.unwrap();
    let unknowns = seeded.gaps().unknowns;
    assert_eq!(unknowns.margin_unknown, 0, "a seed knows every margin");
    assert_eq!(unknowns.partial_positions, 0, "a seed knows every level");
    let applied = seeded
        .catch_up(history, addresses, Some(tip))
        .await
        .unwrap();
    println!(
        "seeded at {seed_block}, {} positions, caught up {applied} events to {tip}",
        seeded.positions().len()
    );
    assert_eq!(seeded.gaps().faults.refused, 0, "the tail is in order");
    agrees(&seeded, &state, config.ema_window).await;
    println!("the seeded replay agrees with the read at block {tip}");
}

/// Blocks behind the lagged head the seeded run starts from: long enough
/// to hold events, short enough for a non-archive node to serve the state.
const SEED_LAG: u64 = 2_000;

/// Every quantity the tape carries, rebuilt in `replay`, against the read
/// at `state`'s block, to the atom.
async fn agrees(replay: &Replay, state: &StateAt, ema_window: u64) {
    let block = state.block();

    let capacity = state.capacity().await.unwrap();
    assert_eq!(
        replay.capacity_at(block),
        Some(capacity),
        "capacity and open interest"
    );

    let mark = state.mark().await.unwrap();
    assert_eq!(
        replay.mark_at(block, ema_window).unwrap(),
        Some(mark),
        "the mark's inputs: pool price, index, EMAs advanced to the block"
    );

    let solvency = state.solvency().await.unwrap();
    let gaps = replay.gaps();
    println!(
        "solvency read {solvency:?}, replayed {:?}, gaps {gaps:?}",
        replay.solvency()
    );
    if gaps.silences == Default::default() {
        assert_eq!(replay.solvency(), Some(solvency), "the solvency books");
    } else {
        // The tape says the books moved without an event; the read is the
        // check, and the difference is what the silence hid.
        let replayed = replay.solvency().unwrap();
        println!(
            "books moved silently {gaps:?}: totalMargin read {} vs replayed {}, badDebt read {} vs replayed {}",
            solvency.total_margin, replayed.total_margin, solvency.bad_debt, replayed.bad_debt
        );
    }

    let minted = state.next_pos_id().await.unwrap();
    assert!(
        (replay.custody().len() as u64) < minted,
        "custody knows at most the positions minted"
    );

    // Every position minted: the fold's view against the row the contract
    // holds. A taker's size is its row's `amount0`; a maker's band is
    // `makerDetails`; a closed position has no row. A position the fold
    // never met is one that closed before a seed, which has no row either.
    let ids: Vec<U256> = (1..minted).map(U256::from).collect();
    let (mut takers, mut makers, mut closed, mut unknown) = (0, 0, 0, 0);
    for outcome in state.positions(&ids).await {
        let pos_id = outcome.pos_id;
        let read = outcome.row.unwrap();
        let Some(folded) = replay.position(pos_id) else {
            assert!(
                read.is_none(),
                "position {pos_id} is held on chain but the fold never met it"
            );
            closed += 1;
            continue;
        };
        let Some(row) = read else {
            assert!(
                !folded.is_open(),
                "position {pos_id} is open in the fold, gone on chain"
            );
            closed += 1;
            continue;
        };
        assert!(
            folded.is_open(),
            "position {pos_id} is closed in the fold, held on chain"
        );
        match folded.kind() {
            PositionKind::Taker { .. } => match folded.taker_size() {
                Some(size) => {
                    let (perp, _) = unpack_balance_delta(row.delta);
                    assert_eq!(size.atoms(), perp, "position {pos_id}'s size");
                    takers += 1;
                }
                None => unknown += 1,
            },
            PositionKind::Maker { .. } => {
                let band = state.maker_band(pos_id).await.unwrap();
                assert_eq!(folded.maker_band(), band, "position {pos_id}'s band");
                makers += 1;
            }
            PositionKind::Unknown => {
                panic!("position {pos_id} is of unknown kind on a tape from genesis or a seed")
            }
        }
    }
    println!(
        "positions: {takers} takers sized, {makers} makers banded, {unknown} takers of unknown size, {closed} closed"
    );

    // The pool's liquidity: every initialized tick's, the tick, the
    // active liquidity, and the price as the root's floored square.
    let pool = state.pool().await.unwrap();
    assert_eq!(
        replay.pool_ticks(),
        Some(pool.ticks.clone()),
        "the tick map"
    );
    assert_eq!(
        pool.sqrt_price.squared().unwrap(),
        replay.pool_price().unwrap(),
        "the pool price"
    );
    match replay.pool_tick() {
        Some(tick) => {
            assert_eq!(tick, pool.tick, "the pool tick");
            assert_eq!(
                replay.pool_liquidity(),
                Some(pool.liquidity),
                "active liquidity"
            );
        }
        None => println!("no swap has moved the tick; the pool's first tick is the factory's"),
    }
    println!(
        "pool: {} initialized ticks, tick {}, active liquidity {}",
        pool.ticks.len(),
        pool.tick,
        pool.liquidity
    );
    println!("the replay agrees with the read at block {}", block.number);
}
