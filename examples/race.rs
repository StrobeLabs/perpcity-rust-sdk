//! End-to-end race benchmark for the PerpCity Rust SDK.
//!
//! Times every phase of a trade lifecycle against an Anvil fork.
//! Designed to run head-to-head with the Zig SDK's equivalent `race.zig`.
//!
//! ```bash
//! export RPC_URL="http://localhost:8545"          # Anvil
//! export PERPCITY_PRIVATE_KEY="ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
//! export PERPCITY_MANAGER="0x..."
//! export PERPCITY_PERP_ID="0x..."
//! export RACE_ITERATIONS="10"
//! cargo run --release --example race
//! ```

use std::env;
use std::time::Instant;

use alloy::primitives::{Address, B256, U256, address};
use alloy::signers::local::PrivateKeySigner;

use perpcity_sdk::*;

const USDC: Address = address!("C1a5D4E99BB224713dd179eA9CA2Fa6600706210");

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.into())
}

#[tokio::main]
async fn main() -> Result<()> {
    let rpc_url = env_or("RPC_URL", "http://localhost:8545");
    let iterations: usize = env_or("RACE_ITERATIONS", "10").parse().unwrap();

    let private_key = env::var("PERPCITY_PRIVATE_KEY").expect("set PERPCITY_PRIVATE_KEY");
    let manager: Address = env::var("PERPCITY_MANAGER")
        .expect("set PERPCITY_MANAGER")
        .parse()
        .unwrap();
    let perp_id: B256 = env::var("PERPCITY_PERP_ID")
        .expect("set PERPCITY_PERP_ID")
        .parse()
        .unwrap();

    let mut phase_times: Vec<(&str, f64)> = Vec::new();
    let race_start = Instant::now();

    // ── Phase: init ──────────────────────────────────────────────────
    let t = Instant::now();
    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&rpc_url)
            .build()?,
    )?;
    let signer: PrivateKeySigner = private_key.parse().unwrap();
    let deployments = Deployments {
        perp_manager: manager,
        usdc: USDC,
        fees_module: None,
        margin_ratios_module: None,
        lockup_period_module: None,
        sqrt_price_impact_limit_module: None,
    };
    // Base Sepolia chain ID = 84532
    let client = PerpClient::new(transport, signer, deployments, 84532)?;
    phase_times.push(("init", ms(t)));

    // ── Phase: sync_nonce ────────────────────────────────────────────
    let t = Instant::now();
    client.sync_nonce().await?;
    phase_times.push(("sync_nonce", ms(t)));

    // ── Phase: refresh_gas ───────────────────────────────────────────
    let t = Instant::now();
    client.refresh_gas().await?;
    phase_times.push(("refresh_gas", ms(t)));

    // ── Phase: ensure_approval ───────────────────────────────────────
    let t = Instant::now();
    client.ensure_approval(U256::MAX).await?;
    phase_times.push(("ensure_approval", ms(t)));

    // ── Phase: read_state ────────────────────────────────────────────
    let t = Instant::now();
    let _config = client.get_perp_config(perp_id).await?;
    phase_times.push(("read_state", ms(t)));

    // ── Trade iterations ─────────────────────────────────────────────
    let mut trade_times_ms: Vec<f64> = Vec::with_capacity(iterations);
    let mut reverts = 0usize;

    for i in 0..iterations {
        // Refresh gas every iteration to keep cache warm (mimics real HFT)
        if i > 0 {
            client.refresh_gas().await?;
        }
        // Re-sync nonce after reverts (reverted txs still consume the nonce on Anvil)
        if i > 0 && reverts > 0 {
            client.sync_nonce().await?;
        }

        let t = Instant::now();
        let result = client
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
            .await;
        let elapsed = ms(t);
        trade_times_ms.push(elapsed);

        match &result {
            Ok(result) => {
                let pos_id = result.pos_id;
                if i == 0 {
                    eprintln!("  trade[0] (cold): {elapsed:.2}ms  pos_id={pos_id}");
                }
            }
            Err(e) => {
                reverts += 1;
                if i == 0 {
                    eprintln!("  trade[0] (cold, REVERTED): {elapsed:.2}ms  err={e}");
                }
            }
        }
    }

    if reverts > 0 {
        eprintln!(
            "  note: {reverts}/{iterations} trades reverted (timing still valid — full round-trip measured)"
        );
    }

    let total_ms = ms(race_start);

    // ── Compute stats ────────────────────────────────────────────────
    let cold_trade = trade_times_ms[0];
    let warm_trades: &[f64] = if trade_times_ms.len() > 1 {
        &trade_times_ms[1..]
    } else {
        &trade_times_ms
    };
    let warm_avg = warm_trades.iter().sum::<f64>() / warm_trades.len() as f64;
    let warm_min = warm_trades.iter().cloned().fold(f64::INFINITY, f64::min);
    let warm_max = warm_trades.iter().cloned().fold(0.0f64, f64::max);

    let mut sorted = warm_trades.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let warm_p50 = sorted[sorted.len() / 2];
    let warm_p95 = sorted[(sorted.len() as f64 * 0.95) as usize];

    // ── Output JSON ──────────────────────────────────────────────────
    println!("{{");
    println!("  \"sdk\": \"rust\",");
    println!("  \"iterations\": {iterations},");
    println!("  \"total_ms\": {total_ms:.2},");

    println!("  \"phases\": {{");
    for (i, (name, time)) in phase_times.iter().enumerate() {
        let comma = if i < phase_times.len() - 1 { "," } else { "" };
        println!("    \"{name}\": {time:.2}{comma}");
    }
    println!("  }},");

    println!("  \"trades\": {{");
    println!("    \"cold_ms\": {cold_trade:.2},");
    println!("    \"warm_avg_ms\": {warm_avg:.2},");
    println!("    \"warm_min_ms\": {warm_min:.2},");
    println!("    \"warm_max_ms\": {warm_max:.2},");
    println!("    \"warm_p50_ms\": {warm_p50:.2},");
    println!("    \"warm_p95_ms\": {warm_p95:.2}");
    println!("  }},");

    println!(
        "  \"all_trades_ms\": [{}]",
        trade_times_ms
            .iter()
            .map(|t| format!("{t:.2}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("}}");

    Ok(())
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_nanos() as f64 / 1_000_000.0
}
