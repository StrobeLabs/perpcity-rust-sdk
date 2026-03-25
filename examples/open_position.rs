//! Open a taker (long/short) position on PerpCity.
//!
//! Demonstrates the basic flow:
//! 1. Configure transport with RPC endpoints
//! 2. Initialize PerpClient with signer and deployments
//! 3. Sync nonce + refresh gas (required before first transaction)
//! 4. Ensure USDC approval
//! 5. Query market data (mark price, funding, OI)
//! 6. Open a long taker position
//! 7. Monitor position (PnL, funding, liquidation status)
//! 8. Close the position
//!
//! # Running
//!
//! ```bash
//! # Set these in .env or export them:
//! export RPC_URL="https://sepolia.base.org"
//! export PERPCITY_PRIVATE_KEY="0x..."
//! export PERPCITY_MANAGER="0x..."
//! export PERPCITY_PERP_ID="0x..."
//! cargo run --example open_position
//! ```

use std::env;

use alloy::primitives::{Address, B256, U256, address};
use alloy::signers::local::PrivateKeySigner;

use perpcity_sdk::{
    CloseParams, Deployments, HftTransport, OpenTakerParams, PerpClient, TransportConfig, Urgency,
};

/// Base Sepolia USDC address.
const USDC: Address = address!("C1a5D4E99BB224713dd179eA9CA2Fa6600706210");

/// Load a hex-encoded private key from the environment.
fn load_signer() -> PrivateKeySigner {
    let key = env::var("PERPCITY_PRIVATE_KEY").expect(
        "PERPCITY_PRIVATE_KEY must be set. \
         Use a testnet key — never put real funds at risk in examples.",
    );
    key.parse::<PrivateKeySigner>()
        .expect("invalid private key hex")
}

/// Load contract deployment addresses from the environment.
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

/// Load the perp ID (bytes32 pool identifier) from the environment.
fn load_perp_id() -> B256 {
    env::var("PERPCITY_PERP_ID")
        .expect("PERPCITY_PERP_ID must be set")
        .parse::<B256>()
        .expect("invalid PERPCITY_PERP_ID (expected 0x-prefixed 32-byte hex)")
}

#[tokio::main]
async fn main() -> perpcity_sdk::Result<()> {
    // ── 1. Transport ────────────────────────────────────────────────
    dotenvy::dotenv().ok();
    let rpc_url = env::var("RPC_URL").unwrap_or_else(|_| "https://sepolia.base.org".into());

    let transport = HftTransport::new(TransportConfig::builder().endpoint(&rpc_url).build()?)?;

    // ── 2. Client ───────────────────────────────────────────────────
    let signer = load_signer();
    let deployments = load_deployments();
    let perp_id = load_perp_id();

    let client = PerpClient::new(transport, signer, deployments, 84532)?;
    println!("PerpClient initialized for address: {}", client.address());

    // ── 3. Sync nonce + gas (required before any transaction) ──────
    client.sync_nonce().await?;
    client.refresh_gas().await?;
    println!("Nonce synced, gas cache refreshed");

    // ── 4. USDC approval ────────────────────────────────────────────
    // Approve the PerpManager to spend our USDC. Uses infinite approval
    // (U256::MAX) so this only needs to happen once per wallet.
    match client.ensure_approval(U256::from(100_000_000u64)).await? {
        Some(tx_hash) => println!("USDC approved, tx: {tx_hash}"),
        None => println!("USDC already approved"),
    }

    // ── 5. Query market data ────────────────────────────────────────
    let perp_data = client.get_perp_config(perp_id).await?;
    println!("\n=== Market: {} ===", perp_id);
    println!("  Mark price:      {:.6}", perp_data.mark);
    println!("  Tick spacing:    {}", perp_data.tick_spacing);
    println!(
        "  Max leverage:    {:.0}x",
        perp_data.bounds.max_taker_leverage
    );
    println!("  Min margin:      {:.2} USDC", perp_data.bounds.min_margin);
    println!(
        "  Creator fee:     {:.4}%",
        perp_data.fees.creator_fee * 100.0
    );
    println!("  LP fee:          {:.4}%", perp_data.fees.lp_fee * 100.0);

    let funding = client.get_funding_rate(perp_id).await?;
    println!("  Daily funding:   {:.6}%", funding * 100.0);

    let oi = client.get_open_interest(perp_id).await?;
    println!("  Long OI:         {:.2} USDC", oi.long_oi);
    println!("  Short OI:        {:.2} USDC", oi.short_oi);

    let balance = client.get_usdc_balance().await?;
    println!("\n  Wallet USDC:     {:.2}", balance);

    // ── 6. Open a long taker position ───────────────────────────────
    let margin = 10.0; // 10 USDC
    let leverage = 5.0; // 5× leverage → 50 USDC notional
    println!("\nOpening LONG {leverage}x with {margin} USDC margin...");

    let params = OpenTakerParams {
        is_long: true,
        margin,
        leverage,
        unspecified_amount_limit: 0, // no slippage limit for this example
    };

    let position_id = client
        .open_taker(perp_id, &params, Urgency::Normal)
        .await?
        .pos_id;
    println!("Position opened! NFT ID: {position_id}");

    // ── 7. Monitor position ─────────────────────────────────────────
    let pos = client.get_position(position_id).await?;
    println!("\n=== Position {position_id} ===");
    println!("  Perp ID:     {}", pos.perpId);
    println!("  Margin:      {} (6-dec)", pos.margin);
    println!("  Perp delta:  {}", pos.entryPerpDelta);
    println!("  USD delta:   {}", pos.entryUsdDelta);

    // Use math helpers for derived values
    let entry_price =
        perpcity_sdk::math::position::entry_price(pos.entryPerpDelta, pos.entryUsdDelta);
    let size = perpcity_sdk::math::position::position_size(pos.entryPerpDelta);
    println!("  Entry price: {entry_price:.6}");
    println!("  Size:        {size:.6}");

    // Live details (PnL, funding, liquidation check)
    let live = client.get_live_details(position_id).await?;
    println!("\n  PnL:              {:.6} USDC", live.pnl);
    println!("  Funding payment:  {:.6} USDC", live.funding_payment);
    println!("  Effective margin: {:.6} USDC", live.effective_margin);
    println!("  Liquidatable:     {}", live.is_liquidatable);

    // ── 8. Close position ───────────────────────────────────────────
    println!("\nClosing position...");
    // Refresh gas before sending another tx (in production, use a block subscription)
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

    println!("Position closed! tx: {}", close_result.tx_hash);
    match close_result.remaining_position_id {
        Some(remaining) => println!("  Partial close — remaining position: {remaining}"),
        None => println!("  Fully closed."),
    }

    println!("\nDone.");
    Ok(())
}
