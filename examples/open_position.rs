//! Open a taker (long/short) position on PerpCity.
//!
//! Demonstrates the basic flow:
//! 1. Configure transport with RPC endpoints
//! 2. Initialize PerpClient with signer and deployments
//! 3. Sync nonce + refresh gas (required before first transaction)
//! 4. Ensure USDC approval
//! 5. Query market data (mark price, funding, OI)
//! 6. Open a long taker position
//! 7. Read the position state
//! 8. Close the position by reversing the taker delta
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
//! cargo run --example open_position
//! ```

use std::env;

use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;

use perpcity_sdk::{
    ARBITRUM_SEPOLIA_POOL_MANAGER, ARBITRUM_SEPOLIA_USDC, AdjustTakerParams, Deployments,
    HftTransport, OpenTakerParams, PerpClient, TransportConfig, Urgency,
};

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
    let perp: Address = env::var("PERPCITY_PERP")
        .expect("PERPCITY_PERP must be set")
        .parse()
        .expect("invalid PERPCITY_PERP address");

    let usdc = env::var("PERPCITY_USDC")
        .ok()
        .map(|s| s.parse::<Address>().expect("invalid PERPCITY_USDC address"))
        .unwrap_or(ARBITRUM_SEPOLIA_USDC);

    Deployments {
        perp,
        usdc,
        pool_manager: ARBITRUM_SEPOLIA_POOL_MANAGER,
    }
}

#[tokio::main]
async fn main() -> perpcity_sdk::Result<()> {
    // ── 1. Transport ────────────────────────────────────────────────
    dotenvy::dotenv().ok();
    let rpc_url =
        env::var("RPC_URL").unwrap_or_else(|_| "https://sepolia-rollup.arbitrum.io/rpc".into());

    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&rpc_url)
            .build()?,
    )?;

    // ── 2. Client ───────────────────────────────────────────────────
    let signer = load_signer();
    let deployments = load_deployments();
    let perp = deployments.perp;

    let client = PerpClient::new_arbitrum_sepolia(transport, signer, deployments)?;
    println!("PerpClient initialized for address: {}", client.address());

    // ── 3. Sync nonce + gas (required before any transaction) ──────
    client.sync_nonce().await?;
    client.refresh_gas().await?;
    println!("Nonce synced, gas cache refreshed");

    // ── 4. USDC approval ────────────────────────────────────────────
    // Approve the Perp contract to spend our USDC.
    match client.ensure_approval(U256::from(100_000_000u64)).await? {
        Some(tx_hash) => println!("USDC approved, tx: {tx_hash}"),
        None => println!("USDC already approved"),
    }

    // ── 5. Query market data ────────────────────────────────────────
    let perp_data = client.get_perp_config().await?;
    println!("\n=== Market: {perp} ===");
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

    let funding = client.get_funding_rate().await?;
    println!("  Daily funding:   {:.6}%", funding * 100.0);

    let oi = client.get_open_interest().await?;
    println!("  Long OI:         {:.2} USDC", oi.long_oi);
    println!("  Short OI:        {:.2} USDC", oi.short_oi);

    let balance = client.get_usdc_balance().await?;
    println!("\n  Wallet USDC:     {balance:.2}");

    // ── 6. Open a long taker position ───────────────────────────────
    let margin = 10.0; // 10 USDC
    let perp_size = 1.0; // notional in perp token units (positive = long)
    println!("\nOpening LONG with {margin} USDC margin, {perp_size} perp size...");

    let params = OpenTakerParams {
        margin,
        perp_delta: perp_size,
        amt1_limit: 0, // no slippage limit for this example
    };

    let position_id = client.open_taker(&params, Urgency::Normal).await?.pos_id;
    println!("Position opened! NFT ID: {position_id}");

    // ── 7. Read position state ──────────────────────────────────────
    // The position's `delta` is a packed Uniswap V4 BalanceDelta; `margin` is
    // in USDC 6-decimal units. Price-impact / live PnL details deferred to the
    // quoting stage — see issue #56.
    let pos = client.get_position(position_id).await?;
    println!("\n=== Position {position_id} ===");
    println!("  Margin (6-dec): {}", pos.margin);
    println!("  Packed delta:   {}", pos.delta);

    // ── 8. Close position (reverse the taker delta) ─────────────────
    println!("\nClosing position by reversing the taker delta...");
    // Refresh gas before sending another tx (in production, use a block subscription)
    client.refresh_gas().await?;

    let close_result = client
        .adjust_taker(
            &AdjustTakerParams {
                pos_id: position_id,
                margin_delta: 0.0,
                perp_delta: -perp_size,
                amt1_limit: u128::MAX,
            },
            Urgency::Normal,
        )
        .await?;

    println!("Position closed! tx: {}", close_result.tx_hash);

    println!("\nDone.");
    Ok(())
}
