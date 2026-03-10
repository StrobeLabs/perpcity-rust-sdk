//! Open a maker (LP) position on PerpCity.
//!
//! Demonstrates the maker flow:
//! 1. Query the current mark price
//! 2. Calculate a ±5% price range centered around mark
//! 3. Estimate liquidity for the desired margin
//! 4. Open the maker position
//!
//! **Note:** Makers are currently subject to a 7-day lockup, so the position
//! cannot be closed immediately after opening.
//!
//! # Running
//!
//! ```bash
//! # Set these in .env or export them:
//! export RPC_URL="https://sepolia.base.org"
//! export PERPCITY_PRIVATE_KEY="0x..."
//! export PERPCITY_MANAGER="0x..."
//! export PERPCITY_PERP_ID="0x..."
//! cargo run --example open_maker
//! ```

use alloy::primitives::{Address, B256, U256, address};
use alloy::signers::local::PrivateKeySigner;
use perpcity_sdk::math::liquidity::estimate_liquidity;
use perpcity_sdk::math::tick::{align_tick_down, align_tick_up, price_to_tick};
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
    let transport = HftTransport::new(TransportConfig::builder().endpoint(&rpc_url).build()?)?;

    let signer: PrivateKeySigner = env::var("PERPCITY_PRIVATE_KEY")
        .expect("set PERPCITY_PRIVATE_KEY")
        .parse()
        .unwrap();

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

    let mark = client.get_mark_price(perp_id).await?;
    println!("mark price: {mark:.2}");

    // -- Calculate tick range (±5% around mark) --
    let tick_spacing = constants::TICK_SPACING;
    let price_lower = mark * 0.95;
    let price_upper = mark * 1.05;

    let tick_lower = align_tick_down(price_to_tick(price_lower)?, tick_spacing);
    let tick_upper = align_tick_up(price_to_tick(price_upper)?, tick_spacing);
    println!("range: {price_lower:.2} – {price_upper:.2} (ticks {tick_lower}..{tick_upper})");

    // -- Estimate liquidity for 100 USDC margin --
    let margin = 100.0;
    let margin_scaled = (margin * 1_000_000.0) as u128;
    let liquidity_u256 = estimate_liquidity(tick_lower, tick_upper, margin_scaled)?;
    let max_u120: u128 = (1u128 << 120) - 1;
    let liquidity: u128 = u128::try_from(liquidity_u256)
        .unwrap_or(max_u120)
        .min(max_u120);
    println!("liquidity: {liquidity}");

    // -- Open maker position --
    let pos_id = client
        .open_maker(
            perp_id,
            &OpenMakerParams {
                margin,
                price_lower,
                price_upper,
                liquidity,
                max_amt0_in: u128::MAX,
                max_amt1_in: u128::MAX,
            },
            Urgency::Normal,
        )
        .await?;
    println!("opened maker position {pos_id}");
    println!("note: this position is subject to a 7-day lockup before it can be closed");

    Ok(())
}
