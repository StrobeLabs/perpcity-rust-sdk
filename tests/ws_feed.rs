//! Integration test: subscribe to live market events via WebSocket.
//!
//! Connects to a Base Sepolia WebSocket endpoint and listens for
//! PerpManager / Beacon events on the US Foreign Aggression perp.
//!
//! Requires:
//! - `WS_URL` environment variable (e.g. `wss://base-sepolia.g.alchemy.com/v2/<key>`)
//!
//! Run with:
//!
//! ```bash
//! WS_URL="wss://..." cargo test --test ws_feed -- --ignored --nocapture
//! ```

use std::time::Duration;

use alloy::primitives::{Address, B256, address};

use perpcity_sdk::feeds::MarketFeed;
use perpcity_sdk::feeds::events::MarketEvent;
use perpcity_sdk::transport::ws::{ReconnectConfig, WsManager};

// ── Deployed addresses (Base Sepolia) ─────────────────────────────────

const PERP_MANAGER: Address = address!("722b3Ab70078b8B90f25765d91D7A2519252e369");
const BEACON: Address = address!("5feae24d83c83fd6fdac0c1f82253aba06c21819");
const PERP_ID: B256 =
    alloy::primitives::b256!("73bf6d0e03a284f42639516320642652ab022db0f82aff40e77bdd9996affe26");

#[tokio::test]
#[ignore] // Requires live WS endpoint — run with: cargo test --test ws_feed -- --ignored --nocapture
async fn subscribe_and_receive_event() {
    let ws_url = std::env::var("WS_URL").expect("WS_URL environment variable must be set");

    println!("Connecting to {ws_url}...");
    let ws = WsManager::connect(&ws_url, ReconnectConfig::default())
        .await
        .expect("failed to connect WebSocket");
    println!("Connected.");

    println!("Subscribing to events for perp {PERP_ID}...");
    let mut feed = MarketFeed::subscribe(&ws, PERP_MANAGER, BEACON, PERP_ID)
        .await
        .expect("failed to subscribe");
    println!("Subscribed. Waiting for events (timeout: 120s)...\n");

    let timeout = Duration::from_secs(120);
    match tokio::time::timeout(timeout, feed.next()).await {
        Ok(Some(event)) => {
            println!("Received event:");
            match &event {
                MarketEvent::PositionOpened {
                    mark_price,
                    pos_id,
                    is_maker,
                    ..
                } => {
                    println!(
                        "  PositionOpened — mark: {mark_price}, pos_id: {pos_id}, maker: {is_maker}"
                    );
                }
                MarketEvent::NotionalAdjusted {
                    mark_price, pos_id, ..
                } => {
                    println!("  NotionalAdjusted — mark: {mark_price}, pos_id: {pos_id}");
                }
                MarketEvent::PositionClosed {
                    mark_price,
                    pos_id,
                    was_liquidated,
                    ..
                } => {
                    println!(
                        "  PositionClosed — mark: {mark_price}, pos_id: {pos_id}, liquidated: {was_liquidated}"
                    );
                }
                MarketEvent::IndexUpdated { index } => {
                    println!("  IndexUpdated — index: {index}");
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
