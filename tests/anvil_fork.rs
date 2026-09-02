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
use alloy::sol;
use alloy::sol_types::SolCall;

use perpcity_sdk::{
    AdjustTakerParams, Deployments, HftTransport, MakerEquityKind, OpenTakerParams, PerpCityError,
    PerpClient, TransactionError, TransportConfig, Urgency,
};

sol! {
    interface IUsdc {
        function balanceOf(address account) external view returns (uint256);
        function mint(address to, uint256 amount) external;
    }
}

// ── Deployed addresses (Arbitrum Sepolia) ──────────────────────────────

// CITI-NYC ("Citibike Active Trips: NYC") on Arbitrum Sepolia — currently the
// only market with maker liquidity, so taker trades can fill.
const PERP: Address = address!("6d4051Ffb71f391a5B4D8643a29Ec6F66F67df50");
// The collateral token the deployed contracts actually use
// (src/config/ExternalAddresses.sol), NOT the canonical Circle USDC.
const USDC: Address = address!("BEF280BefeE2Cb28c20D1E4Cc1da999B4DA0f1fD");
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
            // Automine (one block per tx) so an approval is mined before the
            // next tx is simulated — avoids a pending-approval race.
            .args([
                "--fork-url",
                FORK_URL,
                "--port",
                &port.to_string(),
                "--chain-id",
                &CHAIN_ID.to_string(),
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
                && resp.status().is_success()
            {
                println!("Anvil ready at {}", instance.url);
                return instance;
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

/// Make a JSON-RPC call to Anvil (fire-and-forget, for cheatcodes / txs).
async fn rpc(client: &reqwest::Client, url: &str, method: &str, params: serde_json::Value) {
    client
        .post(url)
        .json(&serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params, "id": 1}))
        .send()
        .await
        .unwrap();
}

/// `eth_call` returning the raw return bytes.
async fn eth_call(client: &reqwest::Client, url: &str, to: Address, data: &[u8]) -> Vec<u8> {
    let resp: serde_json::Value = client
        .post(url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [{
                "to": format!("{to:?}"),
                "data": format!("0x{}", alloy::primitives::hex::encode(data)),
            }, "latest"],
            "id": 1
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let hex = resp["result"].as_str().unwrap_or("0x");
    alloy::primitives::hex::decode(hex.trim_start_matches("0x")).unwrap_or_default()
}

/// Give `who` spendable test-USDC. The collateral token is a Solady-style test
/// ERC20 exposing a public `mint`, so we just mint directly (its namespaced
/// storage layout makes `anvil_setStorageAt` balance-dealing impractical).
async fn deal_usdc(anvil_url: &str, who: Address, amount: U256) {
    let client = reqwest::Client::new();
    let who_s = format!("{who:?}");

    // Impersonate `who` so `eth_sendTransaction` from it is accepted even when
    // it isn't an unlocked Anvil account. `who` already has ETH (see deal_eth).
    rpc(
        &client,
        anvil_url,
        "anvil_impersonateAccount",
        serde_json::json!([who_s]),
    )
    .await;
    rpc(
        &client,
        anvil_url,
        "eth_sendTransaction",
        serde_json::json!([{
            "from": who_s,
            "to": format!("{USDC:?}"),
            "data": format!("0x{}", alloy::primitives::hex::encode(IUsdc::mintCall { to: who, amount }.abi_encode())),
            "gas": "0x7a1200",
        }]),
    )
    .await;
    rpc(
        &client,
        anvil_url,
        "anvil_stopImpersonatingAccount",
        serde_json::json!([who_s]),
    )
    .await;

    // Wait for the mint to mine and reflect in the balance.
    for _ in 0..40 {
        let ret = eth_call(
            &client,
            anvil_url,
            USDC,
            &IUsdc::balanceOfCall { account: who }.abi_encode(),
        )
        .await;
        if let Ok(bal) = IUsdc::balanceOfCall::abi_decode_returns(&ret)
            && bal >= amount
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("USDC mint did not reflect in balanceOf within timeout");
}

fn deployments() -> Deployments {
    Deployments {
        perp: PERP,
        usdc: USDC,
        pool_manager: perpcity_sdk::ARBITRUM_SEPOLIA_POOL_MANAGER,
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

    // 8. Open a long taker position (10 USDC margin, small perp size).
    //
    // NOTE: against a real Circle USDC (FiatToken), the brute-forced
    // `deal_usdc` storage write makes `balanceOf` read correctly but the
    // transfer may still revert `TransferFromFailed` — funding spendable USDC
    // on the fork needs an impersonated minter. Tracked separately; the
    // binding/scaling/approval path up to the on-chain transfer is exercised.
    println!("\nOpening LONG with 10 USDC margin...");
    client.refresh_gas().await.unwrap();

    // CITI-NYC mark ≈ 7340 with small maker capacity, so size tiny.
    let params = OpenTakerParams {
        margin: 10.0,
        perp_delta: 0.001,
        amt1_limit: u128::MAX,
    };

    let open_result = client.open_taker(&params, Urgency::Normal).await.unwrap();
    let pos_id = open_result.pos_id;
    println!("Position opened! ID: {pos_id}");
    println!("  tx_hash: {}", open_result.tx_hash);
    println!(
        "  realized perp_delta: {}  usd_delta: {}",
        open_result.perp_delta, open_result.usd_delta
    );

    // Realized swap decoded from the TakerOpened event. Opening a long
    // receives perp (+) and pays USD (-); the realized perp size should match
    // the requested 0.001 closely (small price impact on this tiny trade).
    assert!(
        (open_result.perp_delta - 0.001).abs() < 1e-4,
        "realized perp_delta {} should be ~0.001",
        open_result.perp_delta
    );
    assert!(
        open_result.usd_delta < 0.0,
        "opening a long should pay USD (negative usd_delta), got {}",
        open_result.usd_delta
    );

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
                perp_delta: -0.0005,
                // Reducing a long is a sell: amt1_limit is the MIN USD to
                // receive, so 0 = accept any output (MAX would never be met).
                amt1_limit: 0,
            },
            Urgency::Normal,
        )
        .await
        .unwrap();
    println!("  tx_hash: {}", adjust_result.tx_hash);
    println!(
        "  realized perp_delta: {}  usd_delta: {}",
        adjust_result.perp_delta, adjust_result.usd_delta
    );

    // Realized swap decoded from the TakerAdjusted event. Reducing a long
    // sells perp (negative perp_delta) and receives USD (positive usd_delta).
    assert!(
        (adjust_result.perp_delta - (-0.0005)).abs() < 1e-4 && adjust_result.usd_delta > 0.0,
        "realized adjust deltas wrong: perp_delta={} usd_delta={}",
        adjust_result.perp_delta,
        adjust_result.usd_delta
    );

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

    // 12. Close the position via the close_taker wrapper. The remaining delta
    //     is 0.0005 (opened 0.001, reduced 0.0005), so reversing it lands the
    //     notional on exactly zero — the contract auto-settles equity to the
    //     caller and burns the position NFT.
    println!("\nClosing position...");
    client.refresh_gas().await.unwrap();

    let close_result = client
        .close_taker(pos_id, 0.0005, Urgency::Normal)
        .await
        .unwrap();

    println!("Position closed! tx: {}", close_result.tx_hash);
    println!(
        "  realized perp_delta: {}  usd_delta: {}",
        close_result.perp_delta, close_result.usd_delta
    );

    // Closing a long reverses the delta: sells perp (negative perp_delta) and
    // receives USD (positive usd_delta), decoded from the TakerClosed event.
    assert!(
        close_result.perp_delta < 0.0 && close_result.usd_delta > 0.0,
        "close should sell perp and receive USD, got perp_delta={} usd_delta={}",
        close_result.perp_delta,
        close_result.usd_delta
    );

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

#[tokio::test]
#[ignore] // Requires `anvil` — run with: cargo test --test anvil_fork -- --ignored --nocapture
async fn maker_equities_via_batched_reads() {
    // 1. Start Anvil forking Arbitrum Sepolia
    let anvil = AnvilInstance::fork().await;

    // 2. Setup client (reads only — no funding needed)
    let signer: PrivateKeySigner = ANVIL_KEY.parse().unwrap();
    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&anvil.url)
            .build()
            .unwrap(),
    )
    .unwrap();
    let client = PerpClient::new(transport, signer, deployments(), CHAIN_ID).unwrap();

    // 3. Empty input short-circuits without touching the chain.
    assert!(client.get_maker_equities(&[]).await.unwrap().is_empty());

    // 4. Preview a range of position ids. CITI-NYC has live maker liquidity,
    //    so at least one open maker position must exist among the early ids.
    let pos_ids: Vec<U256> = (1u64..=20).map(U256::from).collect();
    let equities = client.get_maker_equities(&pos_ids).await.unwrap();

    // One outcome per input id, in input order.
    assert_eq!(equities.len(), pos_ids.len());
    for (outcome, &requested) in equities.iter().zip(&pos_ids) {
        assert_eq!(outcome.pos_id, requested, "outcomes keep input order");
    }

    for outcome in &equities {
        let pos_id = outcome.pos_id;
        match &outcome.kind {
            MakerEquityKind::Computed(b) => {
                println!(
                    "pos {pos_id}: margin={:.6} funding={:+.6} lp={:+.6} pnl={:+.6} equity={:+.6}",
                    b.margin_usd(),
                    b.funding_owed_usd(),
                    b.lp_fees_usd(),
                    b.unrealized_pnl_usd(),
                    b.equity(),
                );
                assert!(b.margin_atoms() >= 0, "settled margin is stored unsigned");
                assert!(b.equity().is_finite());
            }
            MakerEquityKind::NotAMaker => {}
            MakerEquityKind::Failed(e) => panic!("pos {pos_id} degraded: {e}"),
        }
    }
    let open_makers = equities
        .iter()
        .filter(|o| matches!(o.kind, MakerEquityKind::Computed(_)))
        .count();
    assert!(
        open_makers > 0,
        "expected at least one open maker among ids 1..=20"
    );

    println!("\n=== Maker equities test passed! ({open_makers} open makers) ===");
}

#[tokio::test]
#[ignore] // Requires `anvil` — run with: cargo test --test anvil_fork -- --ignored --nocapture
async fn liquidation_simulation_returns_typed_reverts() {
    // 1. Start Anvil forking Arbitrum Sepolia
    let anvil = AnvilInstance::fork().await;

    // 2. Setup client (reads only — no funding needed)
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

    // 3. The zero address burns the liquidation fee — rejected before any RPC.
    let err = client
        .simulate_liquidate_maker(U256::from(1u8), Address::ZERO)
        .await
        .unwrap_err();
    assert!(
        matches!(err, PerpCityError::Validation(_)),
        "zero fee recipient must fail typed: {err}"
    );

    // 4. A healthy or non-maker position must come back as a DECODED contract
    //    revert — callers key retry/drop decisions off the error name — never
    //    an opaque ABI error.
    for pos_id in [U256::from(1u8), U256::from(999_999u32)] {
        let err = client
            .simulate_liquidate_maker(pos_id, address)
            .await
            .unwrap_err();
        match err {
            PerpCityError::Transaction(TransactionError::SimulationReverted {
                error_name,
                selector,
                ..
            }) => {
                println!("pos {pos_id}: revert {error_name} ({selector})");
            }
            other => panic!("pos {pos_id}: expected SimulationReverted, got {other}"),
        }
    }

    println!("\n=== Liquidation simulation test passed! ===");
}
