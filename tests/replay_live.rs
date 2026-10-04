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

use alloy::primitives::Address;
use perpcity_sdk::history::{Fold, Replay};
use perpcity_sdk::{ChainReader, HftTransport, TransportConfig};

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
    let block = state.block();

    let capacity = state.capacity().await.unwrap();
    assert_eq!(
        replay.capacity_at(block),
        Some(capacity),
        "capacity and open interest"
    );

    let mark = state.mark().await.unwrap();
    assert_eq!(
        replay.mark_at(block, config.ema_window).unwrap(),
        Some(mark),
        "the mark's inputs: pool price, index, EMAs advanced to the block"
    );

    let solvency = state.solvency().await.unwrap();
    let gaps = replay.gaps();
    println!(
        "solvency read {solvency:?}, replayed {:?}, gaps {gaps:?}",
        replay.solvency()
    );
    if gaps.total_margin_unemitted == 0 && gaps.bad_debt_unemitted == 0 {
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
    println!("replay agrees with the read at block {}", block.number);
}
