//! Integration test: open and close a taker position on a forked Base Sepolia.
//!
//! Requires `anvil` (from Foundry) to be installed.
//!
//! Run with:
//!
//! ```bash
//! cargo test --test anvil_fork -- --nocapture
//! ```

use std::process::{Child, Command};
use std::time::Duration;

use alloy::primitives::{Address, B256, U256, address};
use alloy::signers::local::PrivateKeySigner;

use perpcity_sdk::{
    CloseParams, Deployments, HftTransport, OpenTakerParams, PerpClient, TransportConfig, Urgency,
};

// ── Deployed addresses (Base Sepolia) ─────────────────────────────────

const PERP_MANAGER: Address = address!("722b3Ab70078b8B90f25765d91D7A2519252e369");
const USDC: Address = address!("C1a5D4E99BB224713dd179eA9CA2Fa6600706210");
const CHAIN_ID: u64 = 84532; // Base Sepolia

/// US Foreign Aggression perp
const PERP_ID: &str = "0x73bf6d0e03a284f42639516320642652ab022db0f82aff40e77bdd9996affe26";

/// Anvil's default private key #0 (well-known, test-only).
const ANVIL_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

/// Anvil fork RPC port.
const ANVIL_PORT: u16 = 48545;

// ── Anvil process management ──────────────────────────────────────────

struct AnvilInstance {
    child: Child,
    url: String,
}

impl AnvilInstance {
    async fn fork_base_sepolia() -> Self {
        let url = format!("http://127.0.0.1:{ANVIL_PORT}");
        let child = Command::new("anvil")
            .args([
                "--fork-url",
                "https://sepolia.base.org",
                "--port",
                &ANVIL_PORT.to_string(),
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

        let value = format!("{:#066x}", amount);
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

// ── The test ──────────────────────────────────────────────────────────

#[tokio::test]
async fn open_and_close_taker_on_fork() {
    // 1. Start Anvil forking Base Sepolia
    let anvil = AnvilInstance::fork_base_sepolia().await;

    // 2. Setup client
    let signer: PrivateKeySigner = ANVIL_KEY.parse().unwrap();
    let address = signer.address();
    println!("Test wallet: {address:?}");

    let transport = HftTransport::new(
        TransportConfig::builder()
            .endpoint(&anvil.url)
            .build()
            .unwrap(),
    )
    .unwrap();

    let deployments = Deployments {
        perp_manager: PERP_MANAGER,
        usdc: USDC,
        fees_module: None,
        margin_ratios_module: None,
        lockup_period_module: None,
        sqrt_price_impact_limit_module: None,
    };

    let client = PerpClient::new(transport, signer, deployments, CHAIN_ID).unwrap();

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
    let perp_id: B256 = PERP_ID.parse().unwrap();
    client
        .ensure_approval(U256::from(1_000_000_000u64))
        .await
        .unwrap();
    println!("USDC approved");

    // 7. Read market data
    let mark = client.get_mark_price(perp_id).await.unwrap();
    println!("Mark price: {mark}");
    assert!(mark > 0.0, "mark price should be positive");

    let funding = client.get_funding_rate(perp_id).await.unwrap();
    println!("Daily funding rate: {funding}");

    let oi = client.get_open_interest(perp_id).await.unwrap();
    println!("OI — long: {}, short: {}", oi.long_oi, oi.short_oi);

    // 8. Open a long taker position (10 USDC margin, 2x leverage)
    println!("\nOpening LONG 2x with 10 USDC margin...");
    client.refresh_gas().await.unwrap();

    let params = OpenTakerParams {
        is_long: true,
        margin: 10.0,
        leverage: 2.0,
        unspecified_amount_limit: 0,
    };

    let pos_id = client
        .open_taker(perp_id, &params, Urgency::Normal)
        .await
        .unwrap();
    println!("Position opened! ID: {pos_id}");

    // 9. Read position on-chain
    let pos = client.get_position(pos_id).await.unwrap();
    println!("  Margin: {} (6-dec)", pos.margin);
    println!("  Perp delta: {}", pos.entryPerpDelta);
    println!("  USD delta: {}", pos.entryUsdDelta);

    let entry = perpcity_sdk::math::position::entry_price(pos.entryPerpDelta, pos.entryUsdDelta);
    println!("  Entry price: {entry}");

    // 10. Get live details
    let live = client.get_live_details(pos_id).await.unwrap();
    println!("  PnL: {:.6}", live.pnl);
    println!("  Liquidatable: {}", live.is_liquidatable);
    assert!(
        !live.is_liquidatable,
        "fresh position should not be liquidatable"
    );

    // 11. Close position
    println!("\nClosing position...");
    client.refresh_gas().await.unwrap();

    let close_result = client
        .close_position(
            pos_id,
            &CloseParams {
                min_amt0_out: 0,
                min_amt1_out: 0,
                max_amt1_in: u128::MAX,
            },
            Urgency::Normal,
        )
        .await
        .unwrap();

    println!("Position closed! tx: {}", close_result.tx_hash);
    assert!(
        close_result.remaining_position_id.is_none(),
        "expected full close"
    );

    // 12. Check final balance
    client.invalidate_fast_cache();
    let final_balance = client.get_usdc_balance().await.unwrap();
    println!("\nFinal USDC balance: {final_balance}");
    assert!(final_balance > 990.0, "lost too much USDC: {final_balance}");

    println!("\n=== Test passed! ===");
}
