//! Testnet funding-arb simulation prep for CITI-NYC.
//!
//! This example reads a dedicated local testnet bot key from a chmod-600 file,
//! prints only the wallet address, checks balances/allowance, confirms whether
//! TestnetUSDC is publicly mintable by this wallet, and runs an `eth_call`
//! simulation for the approved small CITI-NYC SHORT openTaker plan. It does not
//! broadcast openTaker/approve transactions.

use std::{env, fs};

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, I256, U256, address};
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::transports::BoxTransport;
use perpcity_sdk::{ContractOpenTakerParams, HftTransport, IERC20, Perp, TransportConfig};
use serde::Serialize;

const DEFAULT_KEY_FILE: &str = "/opt/data/secrets/perpcity/funding-arb-testnet.env";
const DEFAULT_RPC_URL: &str = "https://sepolia-rollup.arbitrum.io/rpc";
const TESTNET_USDC: Address = address!("BEF280BefeE2Cb28c20D1E4Cc1da999B4DA0f1fD");
const CITI_NYC_PERP: Address = address!("6d4051ffb71f391a5b4d8643a29Ec6F66F67df50");
const MARGIN_USDC_6: u128 = 5_000_000;
const SHORT_PERP_DELTA_6: i64 = -1_000;

sol! {
    #[sol(rpc)]
    interface ITestnetUSDC {
        function owner() external view returns (address);
        function mint(address to, uint256 amount) external;
    }
}

#[derive(Debug, Serialize)]
struct SimOutput {
    dry_run: bool,
    chain_id: u64,
    wallet_address: Address,
    key_file: String,
    eth_balance_wei: U256,
    usdc_balance: f64,
    usdc_allowance_to_perp: f64,
    testnet_usdc: Address,
    testnet_usdc_owner: Option<Address>,
    mint_available_to_wallet: bool,
    mint_simulation: SimResult,
    open_taker_plan: OpenTakerPlan,
    open_taker_simulation: SimResult,
    blockers: Vec<String>,
}

#[derive(Debug, Serialize)]
struct OpenTakerPlan {
    perp: Address,
    direction: &'static str,
    margin_usdc: f64,
    perp_delta: f64,
    amt1_limit: u128,
    will_send_tx: bool,
}

#[derive(Debug, Serialize)]
struct SimResult {
    ok: bool,
    error: Option<String>,
}

fn read_private_key(path: &str) -> Result<String, Box<dyn std::error::Error>> {
    let raw = fs::read_to_string(path)?;
    for line in raw.lines() {
        if let Some(value) = line.strip_prefix("PERPCITY_TESTNET_BOT_PRIVATE_KEY=") {
            return Ok(value.trim().to_string());
        }
    }
    Err("key file missing PERPCITY_TESTNET_BOT_PRIVATE_KEY".into())
}

fn provider(rpc_url: &str) -> Result<RootProvider, Box<dyn std::error::Error>> {
    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(rpc_url)
            .build()?,
    )?;
    let rpc_client = RpcClient::new(BoxTransport::new(transport), false);
    Ok(RootProvider::new(rpc_client))
}

fn as_usdc(raw: U256) -> f64 {
    raw.to::<u128>() as f64 / 1_000_000.0
}

fn sanitize_error(err: impl ToString) -> String {
    let mut s = err.to_string();
    if s.len() > 500 {
        s.truncate(500);
        s.push_str("...[truncated]");
    }
    s
}

async fn simulate(
    provider: &RootProvider,
    from: Address,
    to: Address,
    input: alloy::primitives::Bytes,
) -> SimResult {
    let tx = TransactionRequest::default()
        .with_from(from)
        .with_to(to)
        .with_input(input);
    match provider.call(tx).await {
        Ok(_) => SimResult {
            ok: true,
            error: None,
        },
        Err(err) => SimResult {
            ok: false,
            error: Some(sanitize_error(err)),
        },
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key_file =
        env::var("PERPCITY_TESTNET_BOT_KEY_FILE").unwrap_or_else(|_| DEFAULT_KEY_FILE.to_string());
    let rpc_url = env::var("PERPCITY_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.to_string());
    let signer: PrivateKeySigner = read_private_key(&key_file)?.parse()?;
    let wallet_address = signer.address();
    let provider = provider(&rpc_url)?;

    let usdc = IERC20::new(TESTNET_USDC, &provider);
    let testnet_usdc = ITestnetUSDC::new(TESTNET_USDC, &provider);
    let perp = Perp::new(CITI_NYC_PERP, &provider);

    let eth_balance_wei = provider.get_balance(wallet_address).await?;
    let usdc_balance_raw: U256 = usdc.balanceOf(wallet_address).call().await?;
    let allowance_raw: U256 = usdc.allowance(wallet_address, CITI_NYC_PERP).call().await?;
    let owner = testnet_usdc.owner().call().await.ok();

    let mint_amount = U256::from(100_000_000u64);
    let mint_calldata = testnet_usdc
        .mint(wallet_address, mint_amount)
        .calldata()
        .clone();
    let mint_simulation = simulate(&provider, wallet_address, TESTNET_USDC, mint_calldata).await;
    let mint_available_to_wallet = mint_simulation.ok;

    let open_params = ContractOpenTakerParams {
        holder: wallet_address,
        margin: MARGIN_USDC_6,
        perpDelta: I256::try_from(SHORT_PERP_DELTA_6)?,
        amt1Limit: U256::ZERO,
    };
    let open_calldata = perp.openTaker(open_params).calldata().clone();
    let open_taker_simulation =
        simulate(&provider, wallet_address, CITI_NYC_PERP, open_calldata).await;

    let mut blockers = Vec::new();
    if eth_balance_wei.is_zero() {
        blockers.push("wallet_has_zero_testnet_eth_for_gas".to_string());
    }
    if usdc_balance_raw < U256::from(MARGIN_USDC_6) {
        blockers.push("wallet_lacks_5_testnet_usdc_margin".to_string());
    }
    if allowance_raw < U256::from(MARGIN_USDC_6) {
        blockers.push("wallet_lacks_usdc_allowance_to_citi_nyc_perp".to_string());
    }
    if !mint_available_to_wallet {
        blockers.push("testnet_usdc_mint_not_available_to_bot_wallet".to_string());
    }
    if !open_taker_simulation.ok {
        blockers
            .push("open_taker_eth_call_reverted_expected_until_funded_and_approved".to_string());
    }

    let out = SimOutput {
        dry_run: true,
        chain_id: 421_614,
        wallet_address,
        key_file,
        eth_balance_wei,
        usdc_balance: as_usdc(usdc_balance_raw),
        usdc_allowance_to_perp: as_usdc(allowance_raw),
        testnet_usdc: TESTNET_USDC,
        testnet_usdc_owner: owner,
        mint_available_to_wallet,
        mint_simulation,
        open_taker_plan: OpenTakerPlan {
            perp: CITI_NYC_PERP,
            direction: "SHORT",
            margin_usdc: MARGIN_USDC_6 as f64 / 1_000_000.0,
            perp_delta: SHORT_PERP_DELTA_6 as f64 / 1_000_000.0,
            amt1_limit: 0,
            will_send_tx: false,
        },
        open_taker_simulation,
        blockers,
    };

    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
