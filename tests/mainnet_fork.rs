//! Integration test: a `v0.2.2-upgradeable` market, created on the live
//! Arbitrum One factory inside an Anvil fork, read, traded and probed
//! through the SDK.
//!
//! Requires `anvil` (from Foundry) and an Arbitrum One endpoint in
//! `ARBITRUM_FORK_URL` (default: the public sequencer RPC; the fork is at
//! the head, so no archive is needed).
//!
//! ```bash
//! cargo test --test mainnet_fork -- --ignored --nocapture
//! ```

use std::process::{Child, Command};
use std::time::Duration;

use alloy::primitives::{Address, B256, U256, address, keccak256};
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};

use perpcity_sdk::constants::SNAPSHOT_BLOCK_LAG;
use perpcity_sdk::contracts::{Modules, Perp, PerpFactory};
use perpcity_sdk::math::liquidity::estimate_liquidity;
use perpcity_sdk::math::tick::{align_tick_down, align_tick_up, price_to_tick};
use perpcity_sdk::{
    ARBITRUM_CHAIN_ID, ARBITRUM_POOL_MANAGER, ARBITRUM_USDC, ChainDeployments, ChainReader,
    ExactOpenTakerParams, HftTransport, MakerEquityKind, OpenMakerParams, PerpCityError,
    PerpClient, TickRange, TransactionError, TransportConfig, Urgency, UsdcAtoms, constants,
};

sol! {
    interface IUsdc {
        function balanceOf(address account) external view returns (uint256);
    }
}

// ── Live Arbitrum One addresses ───────────────────────────────────────

/// The `v0.2.2-upgradeable` factory (perpcity-deployments, 2026-10-02).
const FACTORY_V022: Address = address!("90C8cb83C4257156bf3eE0C9bB8f164B20A04da1");
/// MEME50, a live build-`58b42b7` market whose modules and beacon the new
/// market borrows: the factory checks the beacon's index, not the registry.
const MODULE_SOURCE: Address = address!("7aDe3421eba2BFBC28C9015b484b2FC93443CF62");
/// Circle USDC's `balanceAndBlacklistStates` mapping slot (FiatTokenV2_2).
const USDC_BALANCES_SLOT: u64 = 9;

/// Anvil's default private key #0 (well-known, test-only).
const ANVIL_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

fn fork_url() -> String {
    std::env::var("ARBITRUM_FORK_URL")
        .unwrap_or_else(|_| "https://arb1.arbitrum.io/rpc".to_string())
}

// ── Anvil process management ──────────────────────────────────────────

struct AnvilInstance {
    child: Child,
    url: String,
}

impl AnvilInstance {
    async fn fork() -> Self {
        let port = 48600;
        let url = format!("http://127.0.0.1:{port}");
        let fork_url = fork_url();
        let child = Command::new("anvil")
            .args([
                "--fork-url",
                &fork_url,
                "--port",
                &port.to_string(),
                "--chain-id",
                &ARBITRUM_CHAIN_ID.to_string(),
                "--timeout",
                "60000",
                "--retries",
                "5",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("failed to start anvil — is it installed? (`foundryup`)");
        let instance = Self { child, url };
        for _ in 0..120 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Ok(resp) = reqwest::Client::new()
                .post(&instance.url)
                .json(&serde_json::json!({
                    "jsonrpc": "2.0", "method": "eth_blockNumber", "params": [], "id": 1
                }))
                .send()
                .await
                && resp.status().is_success()
            {
                println!("Anvil ready at {} (fork of {fork_url})", instance.url);
                return instance;
            }
        }
        panic!("Anvil did not become ready within 60 seconds");
    }
}

impl Drop for AnvilInstance {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── Raw RPC helpers ───────────────────────────────────────────────────

async fn rpc(url: &str, method: &str, params: serde_json::Value) -> serde_json::Value {
    let resp: serde_json::Value = reqwest::Client::new()
        .post(url)
        .json(&serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params, "id": 1 }))
        .send()
        .await
        .expect("rpc request")
        .json()
        .await
        .expect("rpc json");
    assert!(
        resp.get("error").is_none(),
        "{method} failed: {}",
        resp["error"]
    );
    resp["result"].clone()
}

/// Mine past the snapshot lag, so the pinned reads land after the latest
/// transaction rather than on a block the market did not exist in.
async fn mine_past_lag(url: &str) {
    rpc(
        url,
        "anvil_mine",
        serde_json::json!([format!("0x{:x}", SNAPSHOT_BLOCK_LAG + 1)]),
    )
    .await;
}

async fn deal_eth(url: &str, who: Address) {
    rpc(
        url,
        "anvil_setBalance",
        serde_json::json!([format!("{who:?}"), "0x56BC75E2D63100000"]),
    )
    .await;
}

/// Write `amount` into Circle USDC's balance slot for `who`.
async fn deal_usdc(url: &str, who: Address, amount: U256) {
    let slot = keccak256((who, U256::from(USDC_BALANCES_SLOT)).abi_encode());
    rpc(
        url,
        "anvil_setStorageAt",
        serde_json::json!([
            format!("{ARBITRUM_USDC:?}"),
            format!("{slot:?}"),
            format!("{:?}", B256::from(amount)),
        ]),
    )
    .await;
    let data = IUsdc::balanceOfCall { account: who }.abi_encode();
    let ret = rpc(
        url,
        "eth_call",
        serde_json::json!([{ "to": format!("{ARBITRUM_USDC:?}"), "data": format!("0x{}", alloy::hex::encode(data)) }, "latest"]),
    )
    .await;
    let ret = alloy::hex::decode(ret.as_str().unwrap()).unwrap();
    assert_eq!(
        IUsdc::balanceOfCall::abi_decode_returns(&ret).unwrap(),
        amount,
        "USDC balance slot did not take"
    );
}

/// Send `data` to `to` from an impersonated `from`; returns the receipt.
async fn send_as(url: &str, from: Address, to: Address, data: Vec<u8>) -> serde_json::Value {
    let from_s = format!("{from:?}");
    rpc(url, "anvil_impersonateAccount", serde_json::json!([from_s])).await;
    let hash = rpc(
        url,
        "eth_sendTransaction",
        serde_json::json!([{
            "from": from_s,
            "to": format!("{to:?}"),
            "data": format!("0x{}", alloy::hex::encode(data)),
            "gas": "0x1C9C380",
        }]),
    )
    .await;
    rpc(
        url,
        "anvil_stopImpersonatingAccount",
        serde_json::json!([from_s]),
    )
    .await;
    for _ in 0..40 {
        let receipt = rpc(url, "eth_getTransactionReceipt", serde_json::json!([hash])).await;
        if !receipt.is_null() {
            assert_eq!(receipt["status"], "0x1", "transaction reverted: {receipt}");
            return receipt;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("transaction {hash} did not mine");
}

fn chain(transport: HftTransport) -> ChainReader {
    ChainReader::new(
        transport,
        ChainDeployments {
            usdc: ARBITRUM_USDC,
            pool_manager: ARBITRUM_POOL_MANAGER,
        },
        ARBITRUM_CHAIN_ID,
    )
}

/// Create a market on the live `v0.2.2` factory with MEME50's modules and
/// beacon; returns its address from `PerpCreated`.
async fn create_v022_market(client: &PerpClient, owner: Address, anvil_url: &str) -> Address {
    let source = Perp::new(MODULE_SOURCE, client.chain().provider());
    let modules: Modules = source.modules().call().await.expect("modules()");
    let salt = B256::from(U256::from(0x5eed_u64));
    let data = PerpFactory::createPerpCall {
        owner,
        name: "SDK fork test".into(),
        symbol: "SDKV022".into(),
        tokenUri: String::new(),
        modules,
        emaWindow: alloy::primitives::Uint::from(3_600u32),
        salt,
    }
    .abi_encode();
    let receipt = send_as(anvil_url, owner, FACTORY_V022, data).await;
    let perp_created = <PerpFactory::PerpCreated as alloy::sol_types::SolEvent>::SIGNATURE_HASH;
    let log = receipt["logs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|log| log["topics"][0].as_str() == Some(&format!("{perp_created:?}")))
        .expect("PerpCreated log");
    let data = alloy::hex::decode(log["data"].as_str().unwrap()).unwrap();
    let decoded = <PerpFactory::PerpCreated as alloy::sol_types::SolEvent>::abi_decode_data(&data)
        .expect("decode PerpCreated");
    decoded.0
}

/// A liquidation probe's answer must be the named contract revert.
fn expect_revert(label: &str, probe: perpcity_sdk::Result<()>, expected: &str) {
    match probe.unwrap_err() {
        PerpCityError::Transaction(TransactionError::SimulationReverted { error_name, .. }) => {
            println!("{label} probe: {error_name}");
            assert_eq!(error_name, expected);
        }
        other => panic!("{label} probe: expected a typed revert, got {other}"),
    }
}

#[tokio::test]
#[ignore] // Requires `anvil` and an Arbitrum One endpoint.
async fn v022_market_reads_trades_and_liquidation_probes() {
    let anvil = AnvilInstance::fork().await;
    let signer: alloy::signers::local::PrivateKeySigner = ANVIL_KEY.parse().unwrap();
    let address = signer.address();
    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&anvil.url)
            .build()
            .unwrap(),
    )
    .unwrap();
    let chain = chain(transport);

    deal_eth(&anvil.url, address).await;
    deal_usdc(&anvil.url, address, U256::from(100_000_000_000u64)).await;

    // 1. A new market on the live v0.2.2 factory.
    let bootstrap = PerpClient::new(chain.market(MODULE_SOURCE), signer.clone());
    let perp = create_v022_market(&bootstrap, address, &anvil.url).await;
    println!("created v0.2.2 market {perp}");
    mine_past_lag(&anvil.url).await;
    let client = PerpClient::new(chain.market(perp), signer);
    client.sync_nonce().await.unwrap();
    client.chain().refresh_gas().await.unwrap();
    client.ensure_approval(U256::MAX).await.unwrap();

    // 2. The snapshot reads the stored EMAs from slot 11 (no `emas()` here)
    //    and the mark prices from them.
    let (config, snapshot) = client.market().get_snapshot().await.unwrap();
    println!("snapshot: {snapshot:?}");
    assert_eq!(config.perp, perp);
    assert!(snapshot.index_price > 0.0);
    assert!(snapshot.emas.amm_price > 0.0 && snapshot.emas.index > 0.0);
    assert!((snapshot.pool_price - config.pool_price).abs() < 1e-9);

    // 3. The pool reads through the hook: `extsload` sees the guarded pool.
    let pool = client.market().state().await.unwrap().pool().await.unwrap();
    assert!(
        pool.ticks.is_empty(),
        "a fresh pool has no initialized ticks"
    );

    // 4. A maker band, then a taker against it.
    let pool_price = snapshot.pool_price;
    let tick_lower = align_tick_down(
        price_to_tick(pool_price * 0.8).unwrap(),
        constants::TICK_SPACING,
    );
    let tick_upper = align_tick_up(
        price_to_tick(pool_price * 1.25).unwrap(),
        constants::TICK_SPACING,
    );
    let margin = 5_000.0;
    let liquidity = estimate_liquidity(
        &TickRange::new(tick_lower, tick_upper).unwrap(),
        UsdcAtoms::try_from(margin).unwrap(),
    )
    .unwrap();
    // The fee cache is short-lived by design; refresh it before each send.
    client.chain().refresh_gas().await.unwrap();
    let maker = client
        .open_maker(
            &OpenMakerParams {
                margin,
                price_lower: pool_price * 0.8,
                price_upper: pool_price * 1.25,
                liquidity,
                max_amt0_in: u128::MAX,
                max_amt1_in: u128::MAX,
            },
            Urgency::Normal,
        )
        .await
        .unwrap();
    println!("maker {} opened in {}", maker.pos_id, maker.tx_hash);

    // 5. The maker probe dispatches the 3-arg selector: a healthy, idle
    //    band answers `NotLiquidatable`, never an empty revert. (Once a
    //    taker draws on it, the whole-band liquidation trips the
    //    utilization gate first, so it is probed before the taker opens.)
    expect_revert(
        "maker",
        client.simulate_liquidate_maker(maker.pos_id, address).await,
        "NotLiquidatable",
    );

    client.chain().refresh_gas().await.unwrap();
    let taker = client
        .open_taker_exact(
            &ExactOpenTakerParams {
                margin: 100_000_000,
                perp_delta: 10_000_000,
                amt1_limit: u128::MAX,
            },
            Urgency::Normal,
        )
        .await
        .unwrap();
    println!("taker {} opened in {}", taker.pos_id, taker.tx_hash);
    mine_past_lag(&anvil.url).await;

    // 6. The maker-equity batch runs on the hooked pool's storage.
    let equities = client
        .market()
        .get_maker_equities(&[maker.pos_id])
        .await
        .unwrap();
    match &equities[0].kind {
        MakerEquityKind::Computed(breakdown) => println!("maker equity: {breakdown:?}"),
        other => panic!("expected a computed maker equity, got {other:?}"),
    }

    // 7. The taker probe, the same way.
    expect_revert(
        "taker",
        client.simulate_liquidate_taker(taker.pos_id, address).await,
        "NotLiquidatable",
    );

    // 8. A full close mines the untailed `TakerClosed`, which the receipt
    //    reader decodes.
    client.chain().refresh_gas().await.unwrap();
    let closed = client
        .close_taker(taker.pos_id, Urgency::Normal)
        .await
        .unwrap();
    println!("taker closed in {}: {closed:?}", closed.tx_hash);
    assert!(closed.perp_delta.atoms() < 0, "the close sold the long");

    println!("\n=== v0.2.2 fork test passed! ===");
}
