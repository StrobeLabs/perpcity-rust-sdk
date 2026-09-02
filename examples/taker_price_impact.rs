//! Quote taker price impact locally from an exact, block-pinned liquidity book.
//!
//! ```bash
//! export RPC_URL="https://sepolia-rollup.arbitrum.io/rpc"
//! export PERPCITY_PRIVATE_KEY="0x..." # only supplies the client's address; no tx is sent
//! export PERPCITY_PERP="0x..."
//! cargo run --release --example taker_price_impact
//! ```

use std::env;

use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;
use perpcity_sdk::constants::TICK_SPACING;
use perpcity_sdk::{
    ARBITRUM_SEPOLIA_POOL_MANAGER, ARBITRUM_SEPOLIA_USDC, Deployments, HftTransport, PerpClient,
    QuoteConstraints, TransportConfig, align_tick_down,
};

#[tokio::main]
async fn main() -> perpcity_sdk::Result<()> {
    dotenvy::dotenv().ok();
    let rpc_url = env::var("RPC_URL").expect("set RPC_URL");
    let signer: PrivateKeySigner = env::var("PERPCITY_PRIVATE_KEY")
        .expect("set PERPCITY_PRIVATE_KEY")
        .parse()
        .expect("invalid private key");
    let perp = address("PERPCITY_PERP");
    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&rpc_url)
            .build()?,
    )?;
    let client = PerpClient::new_arbitrum_sepolia(
        transport,
        signer,
        Deployments {
            perp,
            usdc: ARBITRUM_SEPOLIA_USDC,
            pool_manager: ARBITRUM_SEPOLIA_POOL_MANAGER,
        },
    )?;

    // Every field—including ticks and module bounds—comes from this block hash.
    // Once loaded, all calls below are synchronous and make no RPC requests.
    let market = client.load_taker_market_snapshot().await?;
    println!(
        "snapshot block {} ({})",
        market.block.number, market.block.hash
    );

    // Positive perp is an exact-output buy; negative perp is an exact-input sell.
    // Perp and USDC both use six decimals in this deployment.
    let buy = market.quote_perp(1_000_000)?;
    println!(
        "buy 1 perp: usd_delta={} end_sqrt={} allowed={} limit={:?} amt1Limit@25bps={}",
        buy.usd_delta,
        buy.sqrt_price_after_x96,
        buy.price_impact_allowed,
        buy.limit,
        buy.amt1_limit(25),
    );

    // Size the largest trade toward a target without crossing the module bound.
    // Multiplying sqrt price by 1.0005 targets roughly a 10bp higher price.
    let target = market.sqrt_price_x96 * U256::from(10_005) / U256::from(10_000);
    let toward_target = market.quote_to_price(target, QuoteConstraints::default())?;
    println!(
        "toward target: perp_delta={} usd_delta={} stop={:?} end_sqrt={}",
        toward_target.perp_delta,
        toward_target.usd_delta,
        toward_target.limit,
        toward_target.sqrt_price_after_x96,
    );

    // Liquidity-seeding tools can quote against a hypothetical range without
    // mutating the shared live snapshot.
    let lower = align_tick_down(market.tick - 10 * TICK_SPACING, TICK_SPACING);
    let upper = align_tick_down(market.tick + 10 * TICK_SPACING, TICK_SPACING);
    let seeded = market.with_liquidity_delta(lower, upper, 1_000_000_000)?;
    let seeded_buy = seeded.quote_perp(1_000_000)?;
    println!(
        "same buy after hypothetical liquidity: usd_delta={} end_sqrt={}",
        seeded_buy.usd_delta, seeded_buy.sqrt_price_after_x96,
    );

    Ok(())
}

fn address(name: &str) -> Address {
    env::var(name)
        .unwrap_or_else(|_| panic!("set {name}"))
        .parse()
        .unwrap_or_else(|_| panic!("invalid {name} address"))
}
