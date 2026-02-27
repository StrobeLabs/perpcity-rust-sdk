//! Maker (LP) position with tick range on PerpCity.
//! 
//! ** note that we can't actuall execute this strategy at the moment 
//! because makers are subject to a 7-day lockup. 
//!
//! Demonstrates the maker flow:
//! 1. Query the current mark price and market config
//! 2. Calculate a price range centered around the current mark
//! 3. Estimate liquidity for the desired margin and range
//! 4. Open a maker position
//! 5. Monitor PnL and position details
//! 6. Close when done
//!
//! Maker positions provide liquidity in a price range (like Uniswap V3 LP).
//! They earn LP fees from taker trades that cross through their range, but
//! face impermanent loss if the price moves outside the range.
//!
//! # Running
//!
//! ```bash
//! # Set these in .env or export them:
//! export RPC_URL="https://sepolia.base.org"
//! export PERPCITY_PRIVATE_KEY="0x..."
//! export PERPCITY_MANAGER="0x..."
//! export PERPCITY_PERP_ID="0x..."
//! cargo run --example market_maker
//! ```

use std::env;
use std::time::Duration;

use alloy::primitives::{address, Address, B256, U256};
use alloy::signers::local::PrivateKeySigner;

use perpcity_rust_sdk::{
    CloseParams, Deployments, HftTransport, OpenMakerParams, PerpClient, TransportConfig, Urgency,
};
use perpcity_rust_sdk::math::liquidity::estimate_liquidity;
use perpcity_rust_sdk::math::tick::{align_tick_down, align_tick_up, price_to_tick};

/// Base Sepolia USDC address.
const USDC: Address = address!("C1a5D4E99BB224713dd179eA9CA2Fa6600706210");

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
    let perp_manager: Address = env::var("PERPCITY_MANAGER")
        .expect("PERPCITY_MANAGER must be set")
        .parse()
        .expect("invalid PERPCITY_MANAGER address");

    Deployments {
        perp_manager,
        usdc: USDC,
        fees_module: None,
        margin_ratios_module: None,
        lockup_period_module: None,
        sqrt_price_impact_limit_module: None,
    }
}

fn load_perp_id() -> B256 {
    env::var("PERPCITY_PERP_ID")
        .expect("PERPCITY_PERP_ID must be set")
        .parse::<B256>()
        .expect("invalid PERPCITY_PERP_ID")
}

#[tokio::main]
async fn main() -> perpcity_rust_sdk::Result<()> {
    dotenvy::dotenv().ok();
    let rpc_url = env::var("RPC_URL").unwrap_or_else(|_| "https://sepolia.base.org".into());
    let tick_spacing = perpcity_rust_sdk::constants::TICK_SPACING;

    // ── Setup client ────────────────────────────────────────────────
    let transport = HftTransport::new(
        TransportConfig::builder()
            .endpoint(&rpc_url)
            .build()?,
    )?;

    let client = PerpClient::new(transport, load_signer(), load_deployments(), 84532)?;
    let perp_id = load_perp_id();

    println!("Market Maker — address: {}", client.address());

    client.sync_nonce().await?;
    client.refresh_gas().await?;

    // Ensure USDC approval
    client.ensure_approval(U256::from(200_000_000u64)).await?;

    // ── Query market state ──────────────────────────────────────────
    let mark = client.get_mark_price(perp_id).await?;
    let perp_config = client.get_perp_config(perp_id).await?;
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
    println!("  Price lower:  {price_lower:.6}  →  tick {tick_lower} (aligned from {raw_tick_lower})");
    println!("  Price upper:  {price_upper:.6}  →  tick {tick_upper} (aligned from {raw_tick_upper})");

    // ── Estimate liquidity ──────────────────────────────────────────
    //
    // Convert margin to 6-decimal scaled value for the liquidity formula.
    let margin_scaled = (MARGIN_USDC * 1_000_000.0) as u128;
    let liquidity_u256 = estimate_liquidity(tick_lower, tick_upper, margin_scaled)?;

    // The on-chain liquidity field is uint120, so cap at max u120.
    let max_u120: u128 = (1u128 << 120) - 1;
    let liquidity: u128 = u128::try_from(liquidity_u256).unwrap_or(max_u120).min(max_u120);

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

    let position_id = client.open_maker(perp_id, &params, Urgency::Normal).await?;
    println!("Maker position opened! NFT ID: {position_id}");

    // ── Monitor position ────────────────────────────────────────────
    let pos = client.get_position(position_id).await?;
    println!("\n=== Position Details ===");
    println!("  Margin:      {} (6-dec)", pos.margin);
    println!("  Perp delta:  {}", pos.entryPerpDelta);
    println!("  USD delta:   {}", pos.entryUsdDelta);

    // Check live details
    let live = client.get_live_details(position_id).await?;
    println!("  PnL:         {:.6} USDC", live.pnl);
    println!("  Funding:     {:.6} USDC", live.funding_payment);
    println!("  Eff. margin: {:.6} USDC", live.effective_margin);
    println!("  Liquidatable: {}", live.is_liquidatable);

    // ── Simulated monitoring loop ───────────────────────────────────
    //
    // In production, you'd subscribe to new blocks via WebSocket and
    // refresh state on each block. Here we just poll a few times.
    println!("\nMonitoring for 5 seconds...");
    for i in 1..=5 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        // Invalidate fast cache to get fresh prices
        client.invalidate_fast_cache();
        let mark = client.get_mark_price(perp_id).await?;
        let live = client.get_live_details(position_id).await?;
        println!(
            "  [{i}/5] mark={mark:.6}  pnl={:.4}  margin={:.4}  liq={}",
            live.pnl, live.effective_margin, live.is_liquidatable
        );
    }

    // ── Close position ──────────────────────────────────────────────
    println!("\nClosing maker position...");
    client.refresh_gas().await?;

    let close_result = client
        .close_position(
            position_id,
            &CloseParams {
                min_amt0_out: 0,
                min_amt1_out: 0,
                max_amt1_in: u128::MAX,
            },
            Urgency::Normal,
        )
        .await?;

    println!("Closed! tx: {}", close_result.tx_hash);
    match close_result.remaining_position_id {
        Some(rem) => println!("  Partial close — remaining: {rem}"),
        None => println!("  Fully closed."),
    }

    println!("\nDone.");
    Ok(())
}
