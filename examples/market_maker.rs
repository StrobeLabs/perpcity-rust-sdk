//! Maker (LP) position with tick range on PerpCity.
//!
//! ** note that we can't actually execute the full lifecycle at the moment
//! because makers are subject to a 7-day lockup.
//!
//! Demonstrates the maker flow:
//! 1. Query the current mark price and market config
//! 2. Calculate a price range centered around the current mark
//! 3. Estimate liquidity for the desired margin and range
//! 4. Open a maker position
//! 5. Read the raw position state
//!
//! Maker positions provide liquidity in a price range (like Uniswap V3 LP).
//! They earn LP fees from taker trades that cross through their range, but
//! face impermanent loss if the price moves outside the range.
//!
//! # Running
//!
//! ```bash
//! # Set these in .env or export them:
//! export RPC_URL="https://sepolia-rollup.arbitrum.io/rpc"
//! export PERPCITY_PRIVATE_KEY="0x..."
//! export PERPCITY_PERP="0x..."
//! # optional:
//! export PERPCITY_USDC="0x..."   # defaults to Arbitrum Sepolia USDC
//! cargo run --example market_maker
//! ```

use std::env;
use std::time::Duration;

use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;

use perpcity_sdk::math::liquidity::estimate_liquidity;
use perpcity_sdk::math::tick::{align_tick_down, align_tick_up, price_to_tick};
use perpcity_sdk::{
    ARBITRUM_SEPOLIA_USDC, Deployments, HftTransport, OpenMakerParams, PerpClient, TransportConfig,
    Urgency,
};

/// How far above/below the current price to set the range, as a fraction.
/// 0.05 = ±5% → a 10% total range.
const RANGE_WIDTH_PCT: f64 = 0.05;

/// Margin to deposit (USDC).
const MARGIN_USDC: f64 = 100.0;

fn load_signer() -> PrivateKeySigner {
    env::var("PERPCITY_PRIVATE_KEY")
        .expect("PERPCITY_PRIVATE_KEY must be set")
        .parse::<PrivateKeySigner>()
        .expect("invalid private key hex")
}

fn load_deployments() -> Deployments {
    let perp: Address = env::var("PERPCITY_PERP")
        .expect("PERPCITY_PERP must be set")
        .parse()
        .expect("invalid PERPCITY_PERP address");

    let usdc = env::var("PERPCITY_USDC")
        .ok()
        .map(|s| s.parse::<Address>().expect("invalid PERPCITY_USDC address"))
        .unwrap_or(ARBITRUM_SEPOLIA_USDC);

    Deployments { perp, usdc }
}

#[tokio::main]
async fn main() -> perpcity_sdk::Result<()> {
    dotenvy::dotenv().ok();
    let rpc_url =
        env::var("RPC_URL").unwrap_or_else(|_| "https://sepolia-rollup.arbitrum.io/rpc".into());
    let tick_spacing = perpcity_sdk::constants::TICK_SPACING;

    // ── Setup client ────────────────────────────────────────────────
    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&rpc_url)
            .build()?,
    )?;

    let client = PerpClient::new_arbitrum_sepolia(transport, load_signer(), load_deployments())?;

    println!("Market Maker — address: {}", client.address());

    client.sync_nonce().await?;
    client.refresh_gas().await?;

    // Ensure USDC approval
    client.ensure_approval(U256::from(200_000_000u64)).await?;

    // ── Query market state ──────────────────────────────────────────
    let mark = client.get_mark_price().await?;
    let perp_config = client.get_perp_config().await?;
    let balance = client.get_usdc_balance().await?;

    println!("\n=== Market State ===");
    println!("  Mark price:   {mark:.6}");
    println!("  Tick spacing: {tick_spacing}");
    println!("  LP fee:       {:.4}%", perp_config.fees.lp_fee * 100.0);
    println!("  Wallet USDC:  {balance:.2}");

    // ── Calculate tick range ────────────────────────────────────────
    //
    // Center a range around the current mark price.
    // price_lower = mark * (1 - RANGE_WIDTH_PCT)
    // price_upper = mark * (1 + RANGE_WIDTH_PCT)
    // Then align ticks to the pool's tick spacing.
    let price_lower = mark * (1.0 - RANGE_WIDTH_PCT);
    let price_upper = mark * (1.0 + RANGE_WIDTH_PCT);

    let raw_tick_lower = price_to_tick(price_lower)?;
    let raw_tick_upper = price_to_tick(price_upper)?;

    let tick_lower = align_tick_down(raw_tick_lower, tick_spacing);
    let tick_upper = align_tick_up(raw_tick_upper, tick_spacing);

    println!("\n=== Range Calculation ===");
    println!("  Range width:  ±{:.1}%", RANGE_WIDTH_PCT * 100.0);
    println!(
        "  Price lower:  {price_lower:.6}  →  tick {tick_lower} (aligned from {raw_tick_lower})"
    );
    println!(
        "  Price upper:  {price_upper:.6}  →  tick {tick_upper} (aligned from {raw_tick_upper})"
    );

    // ── Estimate liquidity ──────────────────────────────────────────
    //
    // Convert margin to 6-decimal scaled value for the liquidity formula.
    let margin_scaled = (MARGIN_USDC * 1_000_000.0) as u128;
    let liquidity_u256 = estimate_liquidity(tick_lower, tick_upper, margin_scaled)?;

    // The on-chain liquidity field is uint120, so cap at max u120.
    let max_u120: u128 = (1u128 << 120) - 1;
    let liquidity: u128 = u128::try_from(liquidity_u256)
        .unwrap_or(max_u120)
        .min(max_u120);

    println!("\n=== Liquidity Estimate ===");
    println!("  Margin:       {MARGIN_USDC:.2} USDC ({margin_scaled} scaled)");
    println!("  Liquidity:    {liquidity}");

    // ── Open maker position ─────────────────────────────────────────
    println!("\nOpening maker position...");

    let params = OpenMakerParams {
        margin: MARGIN_USDC,
        price_lower,
        price_upper,
        liquidity,
        max_amt0_in: u128::MAX, // no slippage limit on token0
        max_amt1_in: u128::MAX, // no slippage limit on token1
    };

    let position_id = client.open_maker(&params, Urgency::Normal).await?.pos_id;
    println!("Maker position opened! NFT ID: {position_id}");

    // ── Read position state ─────────────────────────────────────────
    // `delta` is a packed Uniswap V4 BalanceDelta; `margin` is in USDC 6-dec.
    // Price-impact / live PnL details deferred to the quoting stage — see issue #56.
    let pos = client.get_position(position_id).await?;
    println!("\n=== Position Details ===");
    println!("  Margin (6-dec): {}", pos.margin);
    println!("  Packed delta:   {}", pos.delta);

    // ── Simulated monitoring loop ───────────────────────────────────
    //
    // In production, you'd subscribe to new blocks via WebSocket and
    // refresh state on each block. Here we just poll the mark a few times.
    println!("\nMonitoring for 5 seconds...");
    for i in 1..=5 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        // Invalidate fast cache to get fresh prices
        client.invalidate_fast_cache();
        let mark = client.get_mark_price().await?;
        println!("  [{i}/5] mark={mark:.6}");
    }

    println!(
        "\nDone. Note: this maker position is subject to a 7-day lockup before it can be closed."
    );
    Ok(())
}
