//! One-shot Perp City Arbitrum Sepolia funding-arb taker simulator/sender.
//!
//! Default: simulate only. Set SEND_LIVE=1 to send one tiny testnet taker tx.
//! Reads local key file and never prints the private key.

use std::{env, fs, io::Write, path::Path};

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, I256, U256, address};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use perpcity_sdk::{
    ContractOpenTakerParams, Deployments, HftTransport, IERC20, OpenTakerParams, Perp, PerpClient,
    TransportConfig, Urgency,
};

const DEFAULT_KEY_FILE: &str = "/opt/data/secrets/perpcity/funding-arb-testnet.env";
const DEFAULT_RPC_URL: &str = "https://sepolia-rollup.arbitrum.io/rpc";
const TESTNET_USDC: Address = address!("BEF280BefeE2Cb28c20D1E4Cc1da999B4DA0f1fD");
const HRMZ_TT_PERP: Address = address!("A77De9Df6e08BEB8f523153dD0110465190526E3");
const HRMZ_CT_PERP: Address = address!("d802ff15C9D828390dc155BA3908fCbe0E868E62");

fn is_blocked_hormuz_mirror(market: &str, perp: Address) -> bool {
    market.eq_ignore_ascii_case("HRMZ-TT")
        || market.eq_ignore_ascii_case("HRMZ-CT")
        || perp == HRMZ_TT_PERP
        || perp == HRMZ_CT_PERP
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

fn as_usdc(raw: U256) -> String {
    let whole = raw / U256::from(1_000_000u64);
    let frac = raw % U256::from(1_000_000u64);
    format!("{}.{:06}", whole, frac.to::<u64>())
}

fn state_file() -> String {
    env::var("PERPCITY_TAKER_STATE_FILE")
        .unwrap_or_else(|_| "/opt/data/private/perpcity/testnet_taker_once_state.log".to_string())
}

fn already_sent(state_path: &str, id: &str) -> bool {
    fs::read_to_string(state_path)
        .map(|s| s.lines().any(|line| line.contains(id)))
        .unwrap_or(false)
}

fn record_sent(
    state_path: &str,
    id: &str,
    tx_hash: impl std::fmt::Display,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = Path::new(state_path).parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_path)?;
    f.write_all(format!("{id} tx={tx_hash}\n").as_bytes())?;
    Ok(())
}

fn sanitize_error(err: impl ToString) -> String {
    let mut s = err.to_string();
    if s.len() > 240 {
        s.truncate(240);
        s.push_str("...[truncated]");
    }
    s.replace('"', "'")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key_file =
        env::var("PERPCITY_TESTNET_BOT_KEY_FILE").unwrap_or_else(|_| DEFAULT_KEY_FILE.to_string());
    let rpc_url = env::var("PERPCITY_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.to_string());
    let market = env::var("PERPCITY_MARKET_SYMBOL").unwrap_or_else(|_| "MCNT-RDP".to_string());
    let perp_addr: Address = env::var("PERPCITY_PERP")?.parse()?;
    if is_blocked_hormuz_mirror(&market, perp_addr) {
        return Err("blocked: HRMZ-TT/HRMZ-CT are mainnet-mirror markets and must not be traded by generic testnet_taker_once".into());
    }
    let margin = env::var("TAKER_MARGIN_USDC")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(5.0);
    let perp_delta = env::var("TAKER_PERP_DELTA")?.parse::<f64>()?;
    let amt1_limit = env::var("TAKER_AMT1_LIMIT")
        .ok()
        .and_then(|v| v.parse::<u128>().ok())
        .unwrap_or(0);
    let send_live = env::var("SEND_LIVE").ok().as_deref() == Some("1");
    let allow_duplicate = env::var("ALLOW_DUPLICATE_TAKER").ok().as_deref() == Some("1");

    let signer: PrivateKeySigner = read_private_key(&key_file)?.parse()?;
    let wallet_address = signer.address();
    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&rpc_url)
            .build()?,
    )?;
    let client = PerpClient::new_arbitrum_sepolia(
        transport,
        signer,
        Deployments {
            perp: perp_addr,
            usdc: TESTNET_USDC,
        },
    )?;
    client.sync_nonce().await?;
    client.set_gas_ttl(120_000);
    client.refresh_gas().await?;

    let usdc = IERC20::new(TESTNET_USDC, client.provider());
    let balance_raw: U256 = usdc.balanceOf(wallet_address).call().await?;
    let allowance_raw: U256 = usdc.allowance(wallet_address, perp_addr).call().await?;
    let margin_scaled = (margin * 1_000_000.0) as u128;
    let perp_delta_scaled = (perp_delta * 1_000_000_000_000_000_000.0).round() as i128;

    let contract = Perp::new(perp_addr, client.provider());
    let calldata = contract
        .openTaker(ContractOpenTakerParams {
            holder: wallet_address,
            margin: margin_scaled,
            perpDelta: I256::try_from(perp_delta_scaled)?,
            amt1Limit: U256::from(amt1_limit),
        })
        .calldata()
        .clone();
    let sim_tx = TransactionRequest::default()
        .with_from(wallet_address)
        .with_to(perp_addr)
        .with_input(calldata);
    let simulation = client.provider().call(sim_tx).await;
    let simulation_ok = simulation.is_ok();
    let simulation_error = simulation.err().map(sanitize_error);

    let idempotency_key =
        format!("{perp_addr}:{wallet_address}:{margin_scaled}:{perp_delta_scaled}:{amt1_limit}");
    let state_path = state_file();
    let duplicate_blocked =
        send_live && !allow_duplicate && already_sent(&state_path, &idempotency_key);

    let mut approval_tx = None;
    let mut taker_tx = None;
    if send_live && !duplicate_blocked && simulation_ok {
        if allowance_raw < U256::from(margin_scaled) {
            approval_tx = client.ensure_approval(U256::from(margin_scaled)).await?;
        }
        let result = client
            .open_taker(
                &OpenTakerParams {
                    margin,
                    perp_delta,
                    amt1_limit,
                },
                Urgency::Normal,
            )
            .await?;
        record_sent(&state_path, &idempotency_key, result.tx_hash)?;
        taker_tx = Some(result.tx_hash);
    }

    let direction = if perp_delta < 0.0 { "SHORT" } else { "LONG" };
    println!(
        "{{\"market\":\"{market}\",\"perp\":\"{perp_addr}\",\"wallet\":\"{wallet_address}\",\"send_live\":{send_live},\"duplicate_blocked\":{duplicate_blocked},\"direction\":\"{direction}\",\"margin_usdc\":{margin},\"perp_delta\":{perp_delta},\"amt1_limit\":{amt1_limit},\"usdc_balance\":\"{}\",\"usdc_allowance\":\"{}\",\"simulation_ok\":{simulation_ok},\"simulation_error\":{},\"approval_tx\":{},\"taker_tx\":{}}}",
        as_usdc(balance_raw),
        as_usdc(allowance_raw),
        simulation_error
            .map(|e| format!("\"{e}\""))
            .unwrap_or_else(|| "null".to_string()),
        approval_tx
            .map(|h| format!("\"{h}\""))
            .unwrap_or_else(|| "null".to_string()),
        taker_tx
            .map(|h| format!("\"{h}\""))
            .unwrap_or_else(|| "null".to_string())
    );
    Ok(())
}
