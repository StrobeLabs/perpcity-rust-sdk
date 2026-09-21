//! Integration test: list the Arbitrum One perps and read a beacon's
//! newest prints from a live archive RPC.
//!
//! Requires:
//! - `RPC_URL` environment variable, an Arbitrum One endpoint that serves
//!   `eth_getLogs` (e.g. `https://arbitrum.gateway.tenderly.co`)
//!
//! Run with:
//!
//! ```bash
//! RPC_URL="https://..." cargo test --test discovery_live -- --ignored --nocapture
//! ```

use alloy::primitives::{Address, address};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::client::RpcClient;
use alloy::transports::layers::RetryBackoffLayer;

use perpcity_sdk::discovery::list_perps;
use perpcity_sdk::history::latest_beacon_prints;

// The Arbitrum One factory and the block of its first `PerpCreated`.
const FACTORY: Address = address!("CE0c5f65A5eDa69A1dFb3f3273749B649abc4eC6");
const FIRST_CREATION_BLOCK: u64 = 468_050_231;
// HORMUZ, created at block 486214447.
const HORMUZ: Address = address!("137E00487dc079DaD69Ba149994320a8FF4c5b17");

#[tokio::test]
#[ignore] // Requires a live RPC endpoint — run with: cargo test --test discovery_live -- --ignored --nocapture
async fn lists_mainnet_perps_and_reads_beacon_prints() {
    let url = std::env::var("RPC_URL").expect("RPC_URL environment variable must be set");
    // Public gateways rate-limit bursts; the scan returns a 429 to the
    // caller, and a retry layer is where the backoff belongs.
    let client = RpcClient::builder()
        .layer(RetryBackoffLayer::new(10, 1_000, 100))
        .http(url.parse().expect("RPC_URL is a URL"));
    let provider = ProviderBuilder::new().connect_client(client);

    let perps = list_perps(&provider, &[FACTORY], FIRST_CREATION_BLOCK)
        .await
        .expect("list_perps");
    println!("{} perps", perps.len());
    assert!(
        perps.len() >= 48,
        "the factory had created 48 perps by block 506248435"
    );
    let hormuz = perps
        .iter()
        .find(|p| p.perp == HORMUZ)
        .expect("HORMUZ is listed");
    assert_eq!(hormuz.block_number, 486_214_447);

    let head = provider.get_block_number().await.expect("eth_blockNumber");
    let prints = latest_beacon_prints(
        &provider,
        hormuz.modules.beacon,
        hormuz.block_number,
        head,
        10,
    )
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
