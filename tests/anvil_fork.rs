//! Integration test: open and close a taker position on a forked Arbitrum Sepolia.
//!
//! Requires `anvil` (from Foundry) to be installed.
//!
//! Run with:
//!
//! ```bash
//! cargo test --test anvil_fork -- --ignored --nocapture
//! ```

use std::process::{Child, Command};
use std::time::Duration;

use alloy::primitives::{Address, U256, address};
use alloy::signers::local::PrivateKeySigner;

use perpcity_sdk::{
    AdjustTakerParams, Deployments, HftTransport, OpenTakerParams, PerpClient, TransportConfig,
    Urgency,
};

// ── Deployed addresses (Arbitrum Sepolia) ──────────────────────────────

const PERP: Address = address!("722b3Ab70078b8B90f25765d91D7A2519252e369");
const USDC: Address = address!("75faf114eafb1BDbe2F0316DF893fd58CE46AA4d");
const CHAIN_ID: u64 = 421614; // Arbitrum Sepolia

/// Arbitrum Sepolia fork URL.
const FORK_URL: &str = "https://sepolia-rollup.arbitrum.io/rpc";

/// Anvil's default private key #0 (well-known, test-only).
const ANVIL_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

/// Next available Anvil port. Each test gets its own to avoid collisions.
static NEXT_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(48545);

// ── Anvil process management ──────────────────────────────────────────

struct AnvilInstance {
    child: Child,
    url: String,
}

impl AnvilInstance {
    async fn fork() -> Self {
        let port = NEXT_PORT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let url = format!("http://127.0.0.1:{port}");
        let child = Command::new("anvil")
            .args([
                "--fork-url",
                FORK_URL,
                "--port",
                &port.to_string(),
                "--chain-id",
                &CHAIN_ID.to_string(),
                "--block-time",
                "1",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("failed to start anvil — is it installed? (`foundryup`)");

        let instance = Self { child, url };

        // Wait for Anvil to be ready
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Ok(resp) = reqwest::Client::new()
                .post(&instance.url)
                .json(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "eth_blockNumber",
                    "params": [],
                    "id": 1
                }))
                .send()
                .await
            {
                if resp.status().is_success() {
                    println!("Anvil ready at {}", instance.url);
                    return instance;
                }
            }
        }
        panic!("Anvil did not become ready within 15 seconds");
    }
}

impl Drop for AnvilInstance {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── Helpers: fund the test wallet on the fork ─────────────────────────

/// Give `who` ETH for gas via `anvil_setBalance`.
async fn deal_eth(anvil_url: &str, who: Address) {
    let client = reqwest::Client::new();
    client
        .post(anvil_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "anvil_setBalance",
            "params": [format!("{who:?}"), format!("{:#x}", U256::from(10u64).pow(U256::from(18u64)))],
            "id": 1
        }))
        .send()
        .await
        .unwrap();
}

/// Give `who` USDC by finding the correct storage slot and writing directly.
///
/// Tries common ERC20 balance mapping slots until one works.
async fn deal_usdc(anvil_url: &str, who: Address, amount: U256) {
    use alloy::primitives::keccak256;
    let client = reqwest::Client::new();

    // balanceOf(address) selector
    let balance_calldata = format!(
        "0x70a08231000000000000000000000000{}",
        alloy::primitives::hex::encode(who.as_slice())
    );

    for base_slot in [0u64, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 51] {
        // Compute keccak256(abi.encode(address, uint256(slot)))
        let mut data = [0u8; 64];
        data[12..32].copy_from_slice(who.as_slice());
        data[32..64].copy_from_slice(&U256::from(base_slot).to_be_bytes::<32>());
        let storage_slot = keccak256(data);

        let value = format!("{amount:#066x}");
        client
            .post(anvil_url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "anvil_setStorageAt",
                "params": [format!("{USDC:?}"), format!("{storage_slot:?}"), value],
                "id": 2
            }))
            .send()
            .await
            .unwrap();

        // Check if it worked
        let resp: serde_json::Value = client
            .post(anvil_url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "eth_call",
                "params": [{"to": format!("{USDC:?}"), "data": balance_calldata}, "latest"],
                "id": 3
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        if let Some(result) = resp["result"].as_str() {
            let bal = U256::from_str_radix(result.trim_start_matches("0x"), 16).unwrap_or_default();
            if bal >= amount {
                println!("USDC deal succeeded via storage slot {base_slot}");
                return;
            }
        }

        // Reset the slot we tried
        client
            .post(anvil_url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "anvil_setStorageAt",
                "params": [
                    format!("{USDC:?}"),
                    format!("{storage_slot:?}"),
                    "0x0000000000000000000000000000000000000000000000000000000000000000"
                ],
                "id": 4
            }))
            .send()
            .await
            .unwrap();
    }

    panic!("Could not find USDC balance storage slot — tried slots 0-10 and 51");
}

fn deployments() -> Deployments {
    Deployments {
        perp: PERP,
        usdc: USDC,
    }
}

// ── The test ──────────────────────────────────────────────────────────

#[tokio::test]
#[ignore] // Requires `anvil` — run with: cargo test --test anvil_fork -- --ignored --nocapture
async fn open_and_close_taker_on_fork() {
    // 1. Start Anvil forking Arbitrum Sepolia
    let anvil = AnvilInstance::fork().await;

    // 2. Setup client
    let signer: PrivateKeySigner = ANVIL_KEY.parse().unwrap();
    let address = signer.address();
    println!("Test wallet: {address:?}");

    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&anvil.url)
            .build()
            .unwrap(),
    )
    .unwrap();

    let client = PerpClient::new(transport, signer, deployments(), CHAIN_ID).unwrap();

    // 3. Fund the test wallet with ETH (for gas) and USDC
    deal_eth(&anvil.url, address).await;
    deal_usdc(
        &anvil.url,
        address,
        U256::from(1_000_000_000u64), // 1000 USDC
    )
    .await;

    // 4. Initialize client (sync nonce + gas)
    client.sync_nonce().await.unwrap();
    client.refresh_gas().await.unwrap();

    // 5. Check USDC balance
    let balance = client.get_usdc_balance().await.unwrap();
    println!("USDC balance: {balance}");
    assert!(
        balance >= 100.0,
        "expected at least 100 USDC, got {balance}"
    );

    // 6. Approve USDC
    client
        .ensure_approval(U256::from(1_000_000_000u64))
        .await
        .unwrap();
    println!("USDC approved");

    // 7. Read market data
    let mark = client.get_mark_price().await.unwrap();
    println!("Mark price: {mark}");
    assert!(mark > 0.0, "mark price should be positive");

    let funding = client.get_funding_rate().await.unwrap();
    println!("Daily funding rate: {funding}");

    let oi = client.get_open_interest().await.unwrap();
    println!("OI — long: {}, short: {}", oi.long_oi, oi.short_oi);

    // 8. Open a long taker position (10 USDC margin, 1.0 perp size)
    println!("\nOpening LONG with 10 USDC margin...");
    client.refresh_gas().await.unwrap();

    let params = OpenTakerParams {
        margin: 10.0,
        perp_delta: 1.0,
        amt1_limit: 0,
    };

    let open_result = client.open_taker(&params, Urgency::Normal).await.unwrap();
    let pos_id = open_result.pos_id;
    println!("Position opened! ID: {pos_id}");
    println!("  tx_hash: {}", open_result.tx_hash);

    // 9. Read position on-chain. `delta` is a packed BalanceDelta; `margin` is
    //    in USDC 6-decimal units. Price-impact / live position details deferred
    //    to the quoting stage — see issue #56.
    let pos = client.get_position(pos_id).await.unwrap();
    println!("  Margin (6-dec): {}", pos.margin);
    println!("  Packed delta:   {}", pos.delta);
    assert!(pos.margin > 0, "position margin should be positive");

    // 10. Adjust taker — reduce exposure by 0.5 perp (negative perp delta)
    println!("\nAdjusting taker -0.5 perp (reducing long exposure)...");
    client.refresh_gas().await.unwrap();

    let adjust_result = client
        .adjust_taker(
            &AdjustTakerParams {
                pos_id,
                margin_delta: 0.0,
                perp_delta: -0.5,
                amt1_limit: u128::MAX,
            },
            Urgency::Normal,
        )
        .await
        .unwrap();
    println!("  tx_hash: {}", adjust_result.tx_hash);

    // 11. Adjust margin — deposit 2 more USDC (margin-only adjustment)
    println!("\nAdjusting margin +2 USDC...");
    client.refresh_gas().await.unwrap();

    let margin_result = client
        .adjust_taker(
            &AdjustTakerParams {
                pos_id,
                margin_delta: 2.0,
                perp_delta: 0.0,
                amt1_limit: u128::MAX,
            },
            Urgency::Normal,
        )
        .await
        .unwrap();
    println!("  tx_hash: {}", margin_result.tx_hash);

    // 12. Close position by reversing the remaining taker delta.
    println!("\nClosing position...");
    client.refresh_gas().await.unwrap();

    let close_result = client
        .adjust_taker(
            &AdjustTakerParams {
                pos_id,
                margin_delta: 0.0,
                perp_delta: -0.5,
                amt1_limit: u128::MAX,
            },
            Urgency::Normal,
        )
        .await
        .unwrap();

    println!("Position closed! tx: {}", close_result.tx_hash);

    // 13. Check final balance
    client.invalidate_fast_cache();
    let final_balance = client.get_usdc_balance().await.unwrap();
    println!("\nFinal USDC balance: {final_balance}");
    assert!(final_balance > 900.0, "lost too much USDC: {final_balance}");

    println!("\n=== Test passed! ===");
}

#[tokio::test]
#[ignore] // Requires `anvil` — run with: cargo test --test anvil_fork -- --ignored --nocapture
async fn batch_balances_via_multicall() {
    // 1. Start Anvil forking Arbitrum Sepolia
    let anvil = AnvilInstance::fork().await;

    // 2. Setup client
    let signer: PrivateKeySigner = ANVIL_KEY.parse().unwrap();
    let address = signer.address();

    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&anvil.url)
            .build()
            .unwrap(),
    )
    .unwrap();

    let client = PerpClient::new(transport, signer, deployments(), CHAIN_ID).unwrap();

    // 3. Fund test wallet
    deal_eth(&anvil.url, address).await;
    deal_usdc(&anvil.url, address, U256::from(500_000_000u64)).await; // 500 USDC

    // 4. Test get_balances (single address)
    let (usdc, eth) = client.get_balances(address).await.unwrap();
    println!("get_balances: USDC={usdc}, ETH={eth}");
    assert!(usdc >= 500.0, "expected at least 500 USDC, got {usdc}");
    assert!(eth > U256::ZERO, "expected non-zero ETH balance");

    // Cross-check with individual methods
    let usdc_individual = client.get_usdc_balance().await.unwrap();
    assert!(
        (usdc - usdc_individual).abs() < 0.01,
        "multicall USDC ({usdc}) should match individual ({usdc_individual})"
    );

    // 5. Test get_balances_batch (multiple addresses)
    // Create a second address with different balances
    let addr2 = address!("0000000000000000000000000000000000000042");
    deal_eth(&anvil.url, addr2).await;
    deal_usdc(&anvil.url, addr2, U256::from(200_000_000u64)).await; // 200 USDC

    let results = client.get_balances_batch(&[address, addr2]).await.unwrap();
    assert_eq!(results.len(), 2);

    let (usdc1, eth1) = results[0];
    let (usdc2, eth2) = results[1];
    println!("batch[0]: USDC={usdc1}, ETH={eth1}");
    println!("batch[1]: USDC={usdc2}, ETH={eth2}");

    assert!(usdc1 >= 500.0, "addr1 should have >= 500 USDC");
    assert!(usdc2 >= 200.0, "addr2 should have >= 200 USDC");
    assert!(eth1 > U256::ZERO, "addr1 should have ETH");
    assert!(eth2 > U256::ZERO, "addr2 should have ETH");

    // 6. Test empty batch
    let empty = client.get_balances_batch(&[]).await.unwrap();
    assert!(empty.is_empty());

    println!("\n=== Batch balances test passed! ===");
}

#[tokio::test]
#[ignore] // Requires `anvil` — run with: cargo test --test anvil_fork -- --ignored --nocapture
async fn perp_snapshot_via_multicall() {
    // 1. Start Anvil forking Arbitrum Sepolia
    let anvil = AnvilInstance::fork().await;

    // 2. Setup client
    let signer: PrivateKeySigner = ANVIL_KEY.parse().unwrap();
    let address = signer.address();

    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&anvil.url)
            .build()
            .unwrap(),
    )
    .unwrap();

    let client = PerpClient::new(transport, signer, deployments(), CHAIN_ID).unwrap();

    // 3. Fund test wallet (needed for gas if any writes were required)
    deal_eth(&anvil.url, address).await;

    // 4. Fetch snapshot via multicall
    let (perp_data, snapshot) = client.get_perp_snapshot().await.unwrap();

    println!("PerpData:");
    println!("  perp: {}", perp_data.perp);
    println!("  tick_spacing: {}", perp_data.tick_spacing);
    println!("  mark: {}", perp_data.mark);
    println!("  beacon: {:?}", perp_data.beacon);
    println!("  bounds: {:?}", perp_data.bounds);
    println!("  fees: {:?}", perp_data.fees);

    println!("PerpSnapshot:");
    println!("  mark_price: {}", snapshot.mark_price);
    println!("  index_price: {}", snapshot.index_price);
    println!("  funding_rate_daily: {}", snapshot.funding_rate_daily);
    println!(
        "  OI — long: {}, short: {}",
        snapshot.open_interest.long_oi, snapshot.open_interest.short_oi
    );

    // 5. Verify PerpData
    assert_eq!(perp_data.perp, PERP);
    assert!(
        perp_data.tick_spacing > 0,
        "tick_spacing should be positive"
    );
    assert!(perp_data.mark > 0.0, "mark price should be positive");
    assert_ne!(perp_data.beacon, Address::ZERO, "beacon should not be zero");

    // 6. Verify PerpSnapshot
    assert!(
        snapshot.mark_price > 0.0,
        "snapshot mark price should be positive"
    );
    assert!(snapshot.index_price > 0.0, "index price should be positive");
    // Funding rate can be positive or negative, just check it's finite
    assert!(
        snapshot.funding_rate_daily.is_finite(),
        "funding rate should be finite"
    );

    // 7. Cross-check: mark price from snapshot should match PerpData
    assert!(
        (snapshot.mark_price - perp_data.mark).abs() < 0.0001,
        "snapshot mark ({}) should match perp_data mark ({})",
        snapshot.mark_price,
        perp_data.mark,
    );

    // 8. Cross-check: individual methods should match multicall results
    let mark_individual = client.get_mark_price().await.unwrap();
    assert!(
        (snapshot.mark_price - mark_individual).abs() < 0.01,
        "multicall mark ({}) should match individual ({})",
        snapshot.mark_price,
        mark_individual,
    );

    let funding_individual = client.get_funding_rate().await.unwrap();
    assert!(
        (snapshot.funding_rate_daily - funding_individual).abs() < 0.001,
        "multicall funding ({}) should match individual ({})",
        snapshot.funding_rate_daily,
        funding_individual,
    );

    println!("\n=== Perp snapshot test passed! ===");
}
