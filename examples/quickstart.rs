//! Minimal PerpCity quickstart — open a long, then close it by reversing.
//!
//! ```bash
//! # Set these in .env or export them:
//! export RPC_URL="https://sepolia-rollup.arbitrum.io/rpc"
//! export PERPCITY_PRIVATE_KEY="0x..."
//! export PERPCITY_PERP="0x..."
//! # optional:
//! export PERPCITY_USDC="0x..."   # defaults to Arbitrum Sepolia USDC
//! cargo run --release --example quickstart
//! ```

use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;
use perpcity_sdk::*;
use std::env;

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.into())
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let rpc_url = env_or("RPC_URL", "https://sepolia-rollup.arbitrum.io/rpc");

    // -- Connect --
    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&rpc_url)
            .build()?,
    )?;

    let signer: PrivateKeySigner = env::var("PERPCITY_PRIVATE_KEY")
        .expect("set PERPCITY_PRIVATE_KEY")
        .parse()
        .unwrap();

    let usdc = env::var("PERPCITY_USDC")
        .ok()
        .map(|s| s.parse::<Address>().expect("invalid PERPCITY_USDC address"))
        .unwrap_or(ARBITRUM_SEPOLIA_USDC);

    let deployments = Deployments {
        perp: env::var("PERPCITY_PERP")
            .expect("set PERPCITY_PERP")
            .parse::<Address>()
            .unwrap(),
        usdc,
    };

    let client = PerpClient::new_arbitrum_sepolia(transport, signer, deployments)?;
    println!("connected to {rpc_url}");

    // -- Warm caches --
    client.sync_nonce().await?;
    client.refresh_gas().await?;
    client.ensure_approval(U256::MAX).await?;

    // -- Read market state --
    let config = client.get_perp_config().await?;
    println!("mark price: {:.2}", config.mark);

    // -- Open a long with 10 USDC margin (perp_delta > 0 = long) --
    let open = client
        .open_taker(
            &OpenTakerParams {
                margin: 10.0,
                perp_delta: 1.0,
                amt1_limit: 0,
            },
            Urgency::Normal,
        )
        .await?;
    let pos_id = open.pos_id;
    println!("opened position {pos_id}");

    // -- Close it by adjusting the taker with the opposing perp delta --
    client.refresh_gas().await?;
    let result = client
        .adjust_taker(
            &AdjustTakerParams {
                pos_id,
                margin_delta: 0.0,
                perp_delta: -1.0,
                amt1_limit: u128::MAX,
            },
            Urgency::Normal,
        )
        .await?;
    println!("closed: {}", result.tx_hash);

    Ok(())
}
