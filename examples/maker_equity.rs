//! Preview maker settle equities for a batch of positions, pick the
//! liquidation candidates from those equities, then gate each attempt on
//! the contract's own health check.
//!
//! DRY RUN by default: nothing is broadcast unless `PERPCITY_DRY_RUN=0`.
//!
//! ```bash
//! export RPC_URL="https://sepolia-rollup.arbitrum.io/rpc"
//! export PERPCITY_PRIVATE_KEY="0x..." # simulation msg.sender; signs sends when PERPCITY_DRY_RUN=0
//! export PERPCITY_PERP="0x..."
//! export PERPCITY_POS_IDS="1,2,3"     # required: position ids to preview
//! export PERPCITY_DRY_RUN=0           # optional: broadcast the liquidations for real
//! cargo run --release --example maker_equity
//! ```

use std::env;

use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;
use perpcity_sdk::{
    ARBITRUM_SEPOLIA_POOL_MANAGER, ARBITRUM_SEPOLIA_USDC, Deployments, HftTransport,
    MakerEquityBreakdown, MakerEquityKind, Perp, PerpCityError, PerpClient, TransactionError,
    TransportConfig, Urgency,
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
    let pos_ids: Vec<U256> = env::var("PERPCITY_POS_IDS")
        .expect("set PERPCITY_POS_IDS to a comma-separated id list, e.g. \"1,2,3\"")
        .split(',')
        .map(|id| id.trim().parse().expect("invalid position id"))
        .collect();
    // DRY RUN unless explicitly disabled: previews and health checks are
    // read-only; only PERPCITY_DRY_RUN=0 broadcasts.
    let dry_run = env::var("PERPCITY_DRY_RUN")
        .map(|v| v != "0")
        .unwrap_or(true);

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
    if !dry_run {
        client.sync_nonce().await?;
        client.refresh_gas().await?;
    }

    // ── 1. One batched, block-pinned read ───────────────────────────
    // Every requested id comes back exactly once, in input order: the
    // settle preview each open maker would receive if touched now, priced
    // at the pinned poolState mark.
    let equities = client.get_maker_equities(&pos_ids).await?;
    let mut candidates: Vec<(U256, &MakerEquityBreakdown)> = Vec::new();
    for outcome in &equities {
        let pos_id = outcome.pos_id;
        match &outcome.kind {
            MakerEquityKind::Computed(b) => {
                println!(
                    "pos {pos_id}: equity={:+.6} settled_margin={:+.6} \
                     funding={:+.6} util={:+.6} lp_fees={:+.6} pnl={:+.6} \
                     value={:.6} ratio={:.4} (liq at {:.4})",
                    b.equity(),
                    b.settled_margin(),
                    b.funding_owed_usd(),
                    b.long_util_earnings_usd() + b.short_util_earnings_usd(),
                    b.lp_fees_usd(),
                    b.unrealized_pnl_usd(),
                    b.position_value_usd(),
                    b.margin_ratio(),
                    b.liq_margin_ratio(),
                );
                candidates.push((pos_id, b));
            }
            MakerEquityKind::NotAMaker => println!("pos {pos_id}: not an open maker"),
            MakerEquityKind::Failed(e) => println!("pos {pos_id}: read failed: {e}"),
        }
    }

    // ── 2. Pick candidates from the equities just computed ──────────
    // Coarse pre-filter so the health probe only runs where the numbers
    // already look thin. `is_liquidatable` mirrors the contract's own
    // check (`PerpLogic.isHealthy`): live equity net of the liquidation
    // fee, over the band's value, against the ratio stored on THAT
    // position — not the market-wide taker ratio, and not the margin.
    // The contract remains the oracle — the filter only saves eth_calls on
    // obviously healthy positions, so it keeps anything near the line.
    let liq_fee = client.get_perp_config().await?.fees.liquidation_fee;
    candidates.retain(|(_, b)| {
        b.is_liquidatable(liq_fee) || b.margin_ratio() < b.liq_margin_ratio() * 1.1
    });
    println!(
        "\n{} candidate(s) at or near their liquidation ratio — probing the contract",
        candidates.len()
    );

    // ── 3. Health-check each candidate, then (optionally) send ──────
    // The typed revert says exactly why not: NotLiquidatable = healthy
    // right now, retry later; NonMakerPosition = wrong book, drop the id;
    // transients = keep going.
    let fee_recipient = client.address();
    for (pos_id, _) in candidates {
        match client.simulate_liquidate_maker(pos_id, fee_recipient).await {
            Ok(()) if dry_run => {
                println!("pos {pos_id}: LIQUIDATABLE (dry run — set PERPCITY_DRY_RUN=0 to send)");
            }
            Ok(()) => {
                println!("pos {pos_id}: LIQUIDATABLE — sending");
                let receipt = client
                    .liquidate_maker(pos_id, fee_recipient, Urgency::Critical)
                    .await?;
                println!("pos {pos_id}: liquidated in {}", receipt.transaction_hash);
            }
            Err(PerpCityError::Transaction(e)) if e.is_revert::<Perp::NotLiquidatable>() => {
                println!("pos {pos_id}: healthy right now (NotLiquidatable); retry later");
            }
            Err(PerpCityError::Transaction(e)) if e.is_revert::<Perp::NonMakerPosition>() => {
                println!("pos {pos_id}: not a maker on-chain; dropping");
            }
            Err(PerpCityError::Transaction(TransactionError::SimulationReverted {
                error_name,
                ..
            })) => println!("pos {pos_id}: not liquidatable ({error_name})"),
            Err(e) if e.is_transient() => println!("pos {pos_id}: transient ({e}); retry"),
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

fn address(name: &str) -> Address {
    env::var(name)
        .unwrap_or_else(|_| panic!("set {name}"))
        .parse()
        .unwrap_or_else(|_| panic!("invalid {name} address"))
}
