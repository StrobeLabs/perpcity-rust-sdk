//! Record a market's tape as a file: its raw logs over a block range, with
//! a manifest naming the chain, the market's addresses, the range, the last
//! block's hash and the decoder's version. Then ask the chain whether the
//! recording's end still stands.
//!
//! Read-only: needs no private key.
//!
//! ```bash
//! export RPC_URL="https://arb1.arbitrum.io/rpc"
//! export PERPCITY_PERP="0x..."
//! export PERPCITY_RECORD_FROM=468050231     # the factory's first block, or the market's
//! export PERPCITY_RECORD_DIR=recordings/hormuz-ships
//! cargo run --release --example record
//! ```

use std::env;
use std::path::PathBuf;

use alloy::primitives::Address;
use perpcity_sdk::history::Recording;
use perpcity_sdk::{ChainReader, HftTransport, TransportConfig};

/// The first block of the deployed-era factory: every live market's tape
/// starts at or after it.
const FACTORY_GENESIS: u64 = 468_050_231;

/// How far back the tail check rescans.
const TAIL_BLOCKS: u64 = 2_000;

#[tokio::main]
async fn main() -> perpcity_sdk::Result<()> {
    dotenvy::dotenv().ok();
    let rpc_url = env::var("RPC_URL").expect("set RPC_URL");
    let perp: Address = env::var("PERPCITY_PERP")
        .expect("set PERPCITY_PERP")
        .parse()
        .expect("a market address");
    let from: u64 = env::var("PERPCITY_RECORD_FROM")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(FACTORY_GENESIS);
    let dir: PathBuf = env::var("PERPCITY_RECORD_DIR")
        .unwrap_or_else(|_| format!("recordings/{perp:#x}"))
        .into();

    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&rpc_url)
            .build()?,
    )?;
    let chain = ChainReader::arbitrum(transport);
    let addresses = chain.market(perp).tape_addresses().await?;
    let history = chain.history();

    let recording = history.record(addresses, from, None).await?;
    let manifest = &recording.manifest;
    println!(
        "recorded {} logs of {perp} over blocks {}..={} on chain {}; tip {}; {} undecodable",
        manifest.logs,
        manifest.from_block,
        manifest.to_block,
        manifest.chain_id,
        manifest.tip_hash,
        manifest.undecodable
    );
    recording
        .write(&dir)
        .expect("the recording directory is writable");
    println!("wrote {}", dir.display());

    let read = Recording::read(&dir).expect("the recording reads back");
    let tape = read.tape()?;
    println!("{} tape rows decode from the file with no node", tape.len());

    let tail = read.check_tail(history, TAIL_BLOCKS).await?;
    println!(
        "tail check over {} blocks: tip hash {}, logs {} ({} recorded, {} rescanned)",
        tail.blocks,
        if tail.tip_hash_matches {
            "matches"
        } else {
            "DIFFERS"
        },
        if tail.logs_match { "match" } else { "DIFFER" },
        tail.recorded,
        tail.rescanned
    );
    Ok(())
}
