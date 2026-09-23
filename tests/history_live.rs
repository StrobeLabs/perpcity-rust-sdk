//! Integration test: read a beacon's newest prints from a live archive RPC.
//!
//! Requires:
//! - `RPC_URL` environment variable, an Arbitrum One endpoint that serves
//!   `eth_getLogs` (e.g. `https://arbitrum.gateway.tenderly.co`)
//!
//! Run with:
//!
//! ```bash
//! RPC_URL="https://..." cargo test --test history_live -- --ignored --nocapture
//! ```

use alloy::primitives::{Address, address};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::client::RpcClient;
use alloy::transports::layers::RetryBackoffLayer;

use perpcity_sdk::history::latest_beacon_prints;

// The HORMUZ beacon, and the block its perp was created at.
const HORMUZ_BEACON: Address = address!("1b37de2b5dc8cf5d290d6dcfded11aaa7d0ef884");
const HORMUZ_CREATION_BLOCK: u64 = 486_214_447;

#[tokio::test]
#[ignore] // Requires a live RPC endpoint — run with: cargo test --test history_live -- --ignored --nocapture
async fn reads_the_newest_beacon_prints() {
    let url = std::env::var("RPC_URL").expect("RPC_URL environment variable must be set");
    // Public gateways rate-limit bursts; the scan returns a 429 to the
    // caller, and a retry layer is where the backoff belongs.
    let client = RpcClient::builder()
        .layer(RetryBackoffLayer::new(10, 1_000, 100))
        .http(url.parse().expect("RPC_URL is a URL"));
    let provider = ProviderBuilder::new().connect_client(client);

    let head = provider.get_block_number().await.expect("eth_blockNumber");
    let prints = latest_beacon_prints(&provider, HORMUZ_BEACON, HORMUZ_CREATION_BLOCK, head, 10)
        .await
        .expect("latest_beacon_prints");
    println!("newest prints: {prints:?}");
    assert!(!prints.is_empty(), "the HORMUZ beacon prints");
    assert!(
        prints
            .windows(2)
            .all(|w| w[0].block_number <= w[1].block_number)
    );
}
