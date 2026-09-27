//! Read a perp's market-event tape: the newest events, replayed through
//! the same decoder the live feed uses, with the position-NFT transfers
//! that map position ids to owners over time.
//!
//! Read-only: needs no private key — the [`History`] handle is built from
//! a bare provider.
//!
//! ```bash
//! export RPC_URL="https://sepolia-rollup.arbitrum.io/rpc"
//! export PERPCITY_PERP="0x..."
//! export PERPCITY_TAPE_LIMIT=50   # optional, default 50
//! cargo run --release --example tape
//! ```

use std::env;

use alloy::primitives::Address;
use alloy::providers::ProviderBuilder;
use perpcity_sdk::feeds::events::MarketEvent;
use perpcity_sdk::history::History;

#[tokio::main]
async fn main() -> perpcity_sdk::Result<()> {
    dotenvy::dotenv().ok();
    let rpc_url = env::var("RPC_URL").expect("set RPC_URL");
    let perp: Address = env::var("PERPCITY_PERP")
        .expect("set PERPCITY_PERP")
        .parse()
        .expect("invalid perp address");
    let limit: usize = env::var("PERPCITY_TAPE_LIMIT")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(50);

    let provider = ProviderBuilder::new()
        .connect(&rpc_url)
        .await
        .expect("provider connects");
    // The handle reads to the head minus its lag by default, and keeps the
    // provider's learned eth_getLogs width across calls.
    let history = History::new(provider);

    let tape = history.latest_market_events(perp, 0, None, limit).await?;
    println!("newest {} market events of {perp}:", tape.len());
    for row in &tape {
        println!(
            "  block {:>12} log {:>3} ts {:>10}  {:?}",
            row.block_number, row.log_index, row.timestamp, row.event
        );
    }

    // The tape carries ownership: a position NFT is minted to its owner,
    // moved by mid-life transfers, and burned on close.
    let mut mints = 0u32;
    let mut burns = 0u32;
    let mut handoffs = 0u32;
    for row in &tape {
        if let MarketEvent::PositionTransferred { from, to, .. } = row.event {
            if from == Address::ZERO {
                mints += 1;
            } else if to == Address::ZERO {
                burns += 1;
            } else {
                handoffs += 1;
            }
        }
    }
    println!("position NFTs in this window: {mints} minted, {burns} burned, {handoffs} handed off");
    Ok(())
}
