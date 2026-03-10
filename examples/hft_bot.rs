//! HFT bot: full pipeline with nonce caching, gas caching, state cache,
//! position management, and latency tracking.
//!
//! Demonstrates how to wire together the SDK's HFT infrastructure for a
//! production-grade trading loop:
//!
//! 1. **Multi-endpoint transport** with latency-based routing and circuit breakers
//! 2. **Nonce + gas pipeline** — zero-RPC transaction preparation
//! 3. **State cache** — 2-tier TTL cache for prices/funding/fees/bounds
//! 4. **Position manager** — automated stop-loss / take-profit / trailing-stop
//! 5. **Latency tracker** — rolling-window P50/P95/P99 stats
//!
//! The main loop runs on every new block (~1 second on Base L2):
//! - Refresh gas cache from block header
//! - Invalidate fast-cache entries
//! - Fetch mark price for all tracked perps
//! - Evaluate position triggers (SL/TP/trailing)
//! - Execute triggered closes
//! - Open new positions when the strategy signals
//!
//! # Running
//!
//! ```bash
//! # Set these in .env or export them:
//! export PERPCITY_PRIVATE_KEY="0x..."
//! export PERPCITY_MANAGER="0x..."
//! export PERPCITY_PERP_ID="0x..."
//! export RPC_URL_1="https://sepolia.base.org"
//! export RPC_URL_2="https://base-sepolia-rpc.publicnode.com"  # optional second endpoint
//! cargo run --example hft_bot
//! ```

use std::collections::HashMap;
use std::env;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, B256, U256, address};
use alloy::signers::local::PrivateKeySigner;

use perpcity_rust_sdk::hft::latency::LatencyTracker;
use perpcity_rust_sdk::hft::position_manager::{ManagedPosition, PositionManager, TriggerType};
use perpcity_rust_sdk::transport::config::Strategy;
use perpcity_rust_sdk::{
    Deployments, HftTransport, OpenTakerParams, PerpClient, TransportConfig, Urgency,
};

/// Base Sepolia USDC address.
const USDC: Address = address!("C1a5D4E99BB224713dd179eA9CA2Fa6600706210");

/// Number of blocks to run the HFT loop before exiting.
const MAX_BLOCKS: u32 = 30;

/// Position sizing: margin in USDC per trade.
const TRADE_MARGIN: f64 = 10.0;

/// Leverage for taker positions.
const TRADE_LEVERAGE: f64 = 5.0;

/// Stop-loss distance from entry price (fraction). 0.02 = 2%.
const STOP_LOSS_PCT: f64 = 0.02;

/// Take-profit distance from entry price (fraction). 0.05 = 5%.
const TAKE_PROFIT_PCT: f64 = 0.05;

/// Trailing stop percentage. 0.03 = 3%.
const TRAILING_STOP_PCT: f64 = 0.03;

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

/// Simple momentum signal: compare current price to a moving average.
/// Returns `Some(true)` for long, `Some(false)` for short, `None` for no signal.
fn momentum_signal(prices: &[f64]) -> Option<bool> {
    if prices.len() < 5 {
        return None; // need at least 5 samples
    }
    let recent_avg: f64 = prices[prices.len() - 3..].iter().sum::<f64>() / 3.0;
    let older_avg: f64 = prices[..prices.len() - 3].iter().sum::<f64>() / (prices.len() - 3) as f64;

    let pct_change = (recent_avg - older_avg) / older_avg;
    if pct_change > 0.001 {
        Some(true) // bullish momentum → go long
    } else if pct_change < -0.001 {
        Some(false) // bearish momentum → go short
    } else {
        None // no clear signal
    }
}

#[tokio::main]
async fn main() -> perpcity_rust_sdk::Result<()> {
    dotenvy::dotenv().ok();
    let perp_id = load_perp_id();

    // ── 1. Multi-endpoint transport ─────────────────────────────────
    //
    // Configure two RPC endpoints with latency-based routing.
    // The transport auto-failovers if one endpoint goes down.
    let rpc_1 = env::var("RPC_URL_1").unwrap_or_else(|_| "https://sepolia.base.org".into());
    let rpc_2 = env::var("RPC_URL_2").ok();

    let mut builder = TransportConfig::builder()
        .endpoint(&rpc_1)
        .strategy(Strategy::LatencyBased)
        .request_timeout(Duration::from_millis(2000));

    if let Some(ref url) = rpc_2 {
        builder = builder.endpoint(url);
    }

    let transport = HftTransport::new(builder.build()?)?;

    // Print transport health
    let health = transport.health_status();
    println!("=== Transport Health ===");
    for (i, status) in health.iter().enumerate() {
        println!(
            "  Endpoint {i}: state={:?}  avg_latency={:.1}ms  errors={:.1}%",
            status.state,
            status.avg_latency_ns as f64 / 1_000_000.0,
            status.error_rate * 100.0,
        );
    }

    // ── 2. Client setup ─────────────────────────────────────────────
    let client = PerpClient::new(transport, load_signer(), load_deployments(), 84532)?;
    println!("\nHFT Bot — address: {}", client.address());

    client.sync_nonce().await?;
    client.refresh_gas().await?;
    client.ensure_approval(U256::from(1_000_000_000u64)).await?;

    // ── 3. Initialize HFT infrastructure ────────────────────────────
    let mut pos_manager = PositionManager::new();
    let mut latency_tracker = LatencyTracker::new();
    let mut trigger_buf: Vec<perpcity_rust_sdk::hft::position_manager::TriggerAction> =
        Vec::with_capacity(16);
    let mut price_history: Vec<f64> = Vec::with_capacity(MAX_BLOCKS as usize);
    let mut next_position_id_counter: u64 = 0;

    // Pre-fetch market config (cached for 60s in the slow layer)
    let perp_config = client.get_perp_config(perp_id).await?;
    println!("\n=== Market Config ===");
    println!(
        "  Max leverage: {:.0}x",
        perp_config.bounds.max_taker_leverage
    );
    println!("  Min margin:   {:.2} USDC", perp_config.bounds.min_margin);
    println!("  LP fee:       {:.4}%", perp_config.fees.lp_fee * 100.0);

    let balance = client.get_usdc_balance().await?;
    println!("  Wallet USDC:  {balance:.2}");
    println!("\nStarting HFT loop ({MAX_BLOCKS} blocks)...\n");

    // ── 4. Main trading loop ────────────────────────────────────────
    for block in 0..MAX_BLOCKS {
        let loop_start = Instant::now();

        // 4a. Refresh gas from latest block header
        if let Err(e) = client.refresh_gas().await {
            eprintln!("  [block {block}] gas refresh failed: {e}");
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }

        // 4b. Invalidate fast cache (prices, funding, balance)
        client.invalidate_fast_cache();

        // 4c. Fetch mark price
        let price_start = Instant::now();
        let mark = match client.get_mark_price(perp_id).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("  [block {block}] price fetch failed: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let price_latency = price_start.elapsed();
        latency_tracker.record(price_latency.as_nanos() as u64);

        price_history.push(mark);

        // 4d. Evaluate position triggers
        let perp_bytes: [u8; 32] = perp_id.into();
        let mut prices_map: HashMap<[u8; 32], f64> = HashMap::new();
        prices_map.insert(perp_bytes, mark);

        trigger_buf.clear();
        pos_manager.check_triggers_into(&prices_map, &mut trigger_buf);

        // 4e. Execute triggered closes
        for trigger in &trigger_buf {
            let action_name = match trigger.trigger_type {
                TriggerType::StopLoss => "STOP LOSS",
                TriggerType::TakeProfit => "TAKE PROFIT",
                TriggerType::TrailingStop => "TRAILING STOP",
            };
            println!(
                "  [block {block}] {action_name} triggered for position {} at price {:.6}",
                trigger.position_id, trigger.trigger_price
            );

            // In production, this would close the on-chain position.
            // For the example, we just untrack it.
            // client.close_position(U256::from(trigger.position_id), &CloseParams { ... }, Urgency::High).await?;
            pos_manager.untrack(trigger.position_id);
        }

        // 4f. Strategy: open a new position if we have a signal and no positions
        if pos_manager.count() == 0
            && let Some(is_long) = momentum_signal(&price_history)
        {
            let direction = if is_long { "LONG" } else { "SHORT" };

            // Calculate stop-loss and take-profit levels
            let (stop_loss, take_profit) = if is_long {
                (mark * (1.0 - STOP_LOSS_PCT), mark * (1.0 + TAKE_PROFIT_PCT))
            } else {
                (mark * (1.0 + STOP_LOSS_PCT), mark * (1.0 - TAKE_PROFIT_PCT))
            };

            println!(
                "  [block {block}] Signal: {direction} at {mark:.6} | SL={stop_loss:.6} TP={take_profit:.6}"
            );

            // Open position on-chain
            let tx_start = Instant::now();
            match client
                .open_taker(
                    perp_id,
                    &OpenTakerParams {
                        is_long,
                        margin: TRADE_MARGIN,
                        leverage: TRADE_LEVERAGE,
                        unspecified_amount_limit: 0,
                    },
                    Urgency::High,
                )
                .await
            {
                Ok(on_chain_pos_id) => {
                    let tx_latency = tx_start.elapsed();
                    latency_tracker.record(tx_latency.as_nanos() as u64);

                    // We use the on-chain U256 position ID. For the position
                    // manager (which uses u64 keys), extract the low 64 bits.
                    let pos_id_u64: u64 = on_chain_pos_id.to::<u64>();
                    next_position_id_counter = pos_id_u64;

                    println!(
                        "  [block {block}] Opened {direction} position #{pos_id_u64} \
                             (tx: {tx_latency:.0?})"
                    );

                    // Track in position manager for automated triggers
                    pos_manager.track(ManagedPosition {
                        perp_id: perp_bytes,
                        position_id: pos_id_u64,
                        is_long,
                        entry_price: mark,
                        margin: TRADE_MARGIN,
                        stop_loss: Some(stop_loss),
                        take_profit: Some(take_profit),
                        trailing_stop_pct: Some(TRAILING_STOP_PCT),
                        trailing_stop_anchor: None,
                    });
                }
                Err(e) => {
                    eprintln!("  [block {block}] Failed to open position: {e}");
                }
            }
        }

        // 4g. Print block summary
        let loop_time = loop_start.elapsed();
        println!(
            "  [block {block}] mark={mark:.6}  positions={} in_flight={} loop={loop_time:.0?}",
            pos_manager.count(),
            client.in_flight_count(),
        );

        // Wait for next block (~1s on Base L2)
        let elapsed = loop_start.elapsed();
        if elapsed < Duration::from_secs(1) {
            tokio::time::sleep(Duration::from_secs(1) - elapsed).await;
        }
    }

    // ── 5. Close remaining positions ────────────────────────────────
    println!("\n=== Shutting down ===");
    if pos_manager.count() > 0 {
        println!("Closing {} remaining positions...", pos_manager.count());
        client.refresh_gas().await?;

        // Collect position IDs to close (avoid borrow issues)
        // In production you'd iterate open positions and close each one.
        // check_triggers with empty prices map = no triggers, but we
        // know we have a position open from the counter.
        println!("  Would close position #{next_position_id_counter}");
    } else {
        println!("No open positions to close.");
    }

    // ── 6. Print latency statistics ─────────────────────────────────
    if let Some(stats) = latency_tracker.stats() {
        println!("\n=== Latency Statistics ===");
        println!("  Samples: {}", stats.count);
        println!("  Min:     {:.2} ms", stats.min_ns as f64 / 1_000_000.0);
        println!("  Avg:     {:.2} ms", stats.avg_ns as f64 / 1_000_000.0);
        println!("  P50:     {:.2} ms", stats.p50_ns as f64 / 1_000_000.0);
        println!("  P95:     {:.2} ms", stats.p95_ns as f64 / 1_000_000.0);
        println!("  P99:     {:.2} ms", stats.p99_ns as f64 / 1_000_000.0);
        println!("  Max:     {:.2} ms", stats.max_ns as f64 / 1_000_000.0);
    }

    // ── 7. Print transport health ───────────────────────────────────
    println!("\n=== Final Transport Health ===");
    for (i, status) in client.transport().health_status().iter().enumerate() {
        println!(
            "  Endpoint {i}: state={:?}  avg_latency={:.1}ms  requests={}  errors={:.1}%",
            status.state,
            status.avg_latency_ns as f64 / 1_000_000.0,
            status.total_requests,
            status.error_rate * 100.0,
        );
    }

    println!("\nHFT bot complete.");
    Ok(())
}
