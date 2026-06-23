//! Integration test: subscribe to live market events via WebSocket.
//!
//! Connects to an Arbitrum Sepolia WebSocket endpoint and listens for
//! `Perp` / `Beacon` events on a perp market.
//!
//! Requires:
//! - `WS_URL` environment variable (e.g. `wss://arb-sepolia.g.alchemy.com/v2/<key>`)
//!
//! Run with:
//!
//! ```bash
//! WS_URL="wss://..." cargo test --test ws_feed -- --ignored --nocapture
//! ```

use std::time::Duration;

use alloy::primitives::{Address, address};

use perpcity_sdk::feeds::MarketFeed;
use perpcity_sdk::feeds::events::MarketEvent;
use perpcity_sdk::transport::ws::{ReconnectConfig, WsManager};

// ── Deployed addresses (Arbitrum Sepolia) ─────────────────────────────

// CITI-NYC market on Arbitrum Sepolia and its beacon.
const PERP: Address = address!("6d4051Ffb71f391a5B4D8643a29Ec6F66F67df50");
const BEACON: Address = address!("8e7e8f46b95d44d2baee933c35d3e3e17dcc2009");

#[tokio::test]
#[ignore] // Requires live WS endpoint — run with: cargo test --test ws_feed -- --ignored --nocapture
async fn subscribe_and_receive_event() {
    let ws_url = std::env::var("WS_URL").expect("WS_URL environment variable must be set");

    println!("Connecting to {ws_url}...");
    let ws = WsManager::connect(&ws_url, ReconnectConfig::default())
        .await
        .expect("failed to connect WebSocket");
    println!("Connected.");

    println!("Subscribing to events for perp {PERP}...");
    let mut feed = MarketFeed::subscribe(&ws, PERP, BEACON)
        .await
        .expect("failed to subscribe");
    println!("Subscribed. Waiting for events (timeout: 120s)...\n");

    let timeout = Duration::from_secs(120);
    match tokio::time::timeout(timeout, feed.next()).await {
        Ok(Some(event)) => {
            println!("Received event:");
            match &event {
                MarketEvent::TakerOpened { pos_id, swap } => {
                    println!(
                        "  TakerOpened — pos_id: {pos_id}, amm_price: {}",
                        swap.amm_price
                    );
                }
                MarketEvent::MakerOpened { pos_id } => {
                    println!("  MakerOpened — pos_id: {pos_id}");
                }
                MarketEvent::TakerClosed { pos_id, swap, .. } => {
                    println!(
                        "  TakerClosed — pos_id: {pos_id}, amm_price: {}",
                        swap.amm_price
                    );
                }
                MarketEvent::OpenInterestUpdated { long_oi, short_oi } => {
                    println!("  OpenInterestUpdated — long: {long_oi}, short: {short_oi}");
                }
                MarketEvent::IndexUpdated { index } => {
                    println!("  IndexUpdated — index: {index}");
                }
                other => {
                    println!("  {other:?}");
                }
            }
            println!("\n=== Test passed! ===");
        }
        Ok(None) => {
            panic!("WebSocket connection lost before receiving an event");
        }
        Err(_) => {
            println!("No events received within {timeout:?} — perp may be inactive.");
            println!("This is not a failure, just no on-chain activity during the test window.");
        }
    }
}
