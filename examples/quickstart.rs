//! Minimal PerpCity quickstart — open a long, check PnL, close it.
//!
//! ```bash
//! # Set these in .env or export them:
//! export RPC_URL="https://sepolia.base.org"
//! export PERPCITY_PRIVATE_KEY="0x..."
//! export PERPCITY_MANAGER="0x..."
//! export PERPCITY_PERP_ID="0x..."
//! cargo run --release --example quickstart
//! ```

use alloy::primitives::{Address, B256, U256, address};
use alloy::signers::local::PrivateKeySigner;
use perpcity_sdk::*;
use std::env;

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.into())
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let rpc_url = env_or("RPC_URL", "https://sepolia.base.org");

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

    // Base Sepolia defaults
    let deployments = Deployments {
        perp_manager: env::var("PERPCITY_MANAGER")
            .expect("set PERPCITY_MANAGER")
            .parse::<Address>()
            .unwrap(),
        usdc: address!("C1a5D4E99BB224713dd179eA9CA2Fa6600706210"),
        fees_module: None,
        margin_ratios_module: None,
        lockup_period_module: None,
        sqrt_price_impact_limit_module: None,
    };

    let client = PerpClient::new(transport, signer, deployments, 84532)?;
    println!("connected to {rpc_url}");

    // -- Warm caches --
    client.sync_nonce().await?;
    client.refresh_gas().await?;
    client.ensure_approval(U256::MAX).await?;

    // -- Read market state --
    let perp_id: B256 = env::var("PERPCITY_PERP_ID")
        .expect("set PERPCITY_PERP_ID")
        .parse()
        .unwrap();

    let config = client.get_perp_config(perp_id).await?;
    println!("mark price: {:.2}", config.mark);

    // -- Open a 2x long with 10 USDC margin --
    let pos_id = client
        .open_taker(
            perp_id,
            &OpenTakerParams {
                is_long: true,
                margin: 10.0,
                leverage: 2.0,
                unspecified_amount_limit: 0,
            },
            Urgency::Normal,
        )
        .await?
        .pos_id;
    println!("opened position {pos_id}");

    // -- Close it --
    let result = client
        .close_position(
            pos_id,
            &CloseParams {
                min_amt0_out: 0,
                min_amt1_out: 0,
                max_amt1_in: u128::MAX,
            },
            Urgency::Normal,
        )
        .await?;
    println!("closed: {}", result.tx_hash);

    Ok(())
}
