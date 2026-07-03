//! One-shot Perp City Arbitrum Sepolia maker-liquidity simulator/sender.
//!
//! Default: simulates only. Set SEND_LIVE=1 to send one small testnet maker tx.
//! Reads local key file and never prints the private key.

use std::{env, fs, io::Write, path::Path};

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Signed, U256, address};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use perpcity_sdk::math::liquidity::estimate_liquidity;
use perpcity_sdk::math::tick::{align_tick_down, align_tick_up, price_to_tick};
use perpcity_sdk::{
    ContractOpenMakerParams, Deployments, HftTransport, IBeacon, IERC20, OpenMakerParams, Perp,
    PerpClient, TransportConfig, Urgency,
};

const DEFAULT_KEY_FILE: &str = "/opt/data/secrets/perpcity/funding-arb-testnet.env";
const DEFAULT_RPC_URL: &str = "https://sepolia-rollup.arbitrum.io/rpc";
const TESTNET_USDC: Address = address!("BEF280BefeE2Cb28c20D1E4Cc1da999B4DA0f1fD");
const DEFAULT_PERP: Address = address!("07d340e33cb6b73ddab3a53f4af971921bb55e49"); // MCNT-OAK
const HRMZ_TT_PERP: Address = address!("A77De9Df6e08BEB8f523153dD0110465190526E3");
const HRMZ_CT_PERP: Address = address!("d802ff15C9D828390dc155BA3908fCbe0E868E62");
const Q96_F64: f64 = 79_228_162_514_264_337_593_543_950_336.0;

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

fn run_id(
    perp: Address,
    wallet: Address,
    margin_scaled: u128,
    tick_lower: i32,
    tick_upper: i32,
) -> String {
    format!("{perp}:{wallet}:{margin_scaled}:{tick_lower}:{tick_upper}")
}

fn state_file() -> String {
    env::var("PERPCITY_LIQUIDITY_STATE_FILE").unwrap_or_else(|_| {
        "/opt/data/private/perpcity/testnet_liquidity_once_state.log".to_string()
    })
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
    let line = format!("{id} tx={tx_hash}\n");
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_path)?
        .write_all(line.as_bytes())?;
    Ok(())
}

fn i32_to_i24(v: i32) -> Signed<24, 1> {
    Signed::<24, 1>::try_from(v as i64).unwrap_or(if v < 0 {
        Signed::<24, 1>::MIN
    } else {
        Signed::<24, 1>::MAX
    })
}

fn x96_to_price(raw: U256) -> f64 {
    raw.to_string().parse::<f64>().unwrap_or(0.0) / Q96_F64
}

fn directional_range(mark: f64, index: f64) -> (String, f64, f64) {
    let buffer_bps = env::var("MAKER_RANGE_BUFFER_BPS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(100.0);
    let buffer = buffer_bps / 10_000.0;
    if index > mark {
        (
            "LONG".to_string(),
            mark * (1.0 - buffer),
            index * (1.0 + buffer),
        )
    } else {
        (
            "SHORT".to_string(),
            index * (1.0 - buffer),
            mark * (1.0 + buffer),
        )
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key_file =
        env::var("PERPCITY_TESTNET_BOT_KEY_FILE").unwrap_or_else(|_| DEFAULT_KEY_FILE.to_string());
    let rpc_url = env::var("PERPCITY_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.to_string());
    let perp_addr: Address = env::var("PERPCITY_PERP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PERP);
    let market = env::var("PERPCITY_MARKET_SYMBOL").unwrap_or_else(|_| "MCNT-OAK".to_string());
    if is_blocked_hormuz_mirror(&market, perp_addr) {
        return Err("blocked: HRMZ-TT/HRMZ-CT are mainnet-mirror markets and must not receive generic testnet_liquidity_once".into());
    }
    let margin = env::var("MAKER_MARGIN_USDC")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100.0);
    let send_live = env::var("SEND_LIVE").ok().as_deref() == Some("1");
    let allow_duplicate = env::var("ALLOW_DUPLICATE_LIQUIDITY").ok().as_deref() == Some("1");

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

    let mark = client.get_mark_price().await?;
    let contract = Perp::new(perp_addr, client.provider());
    let modules = contract.modules().call().await?;
    let beacon = IBeacon::new(modules.beacon, client.provider());
    let index = x96_to_price(beacon.index().call().await?);
    let explicit_lower = env::var("MAKER_PRICE_LOWER")
        .ok()
        .and_then(|v| v.parse::<f64>().ok());
    let explicit_upper = env::var("MAKER_PRICE_UPPER")
        .ok()
        .and_then(|v| v.parse::<f64>().ok());
    let (direction, mut price_lower, mut price_upper) =
        if let (Some(lower), Some(upper)) = (explicit_lower, explicit_upper) {
            ("EXPLICIT".to_string(), lower, upper)
        } else {
            directional_range(mark, index)
        };
    if price_lower > price_upper {
        std::mem::swap(&mut price_lower, &mut price_upper);
    }
    let tick_lower = align_tick_down(
        price_to_tick(price_lower)?,
        perpcity_sdk::constants::TICK_SPACING,
    );
    let tick_upper = align_tick_up(
        price_to_tick(price_upper)?,
        perpcity_sdk::constants::TICK_SPACING,
    );
    let margin_scaled = (margin * 1_000_000.0) as u128;
    let liquidity_u256 = estimate_liquidity(tick_lower, tick_upper, margin_scaled)?;
    let max_u120: u128 = (1u128 << 120) - 1;
    let liquidity = u128::try_from(liquidity_u256)
        .unwrap_or(max_u120)
        .min(max_u120);
    let state_path = state_file();
    let idempotency_key = run_id(
        perp_addr,
        wallet_address,
        margin_scaled,
        tick_lower,
        tick_upper,
    );
    let duplicate_blocked =
        send_live && !allow_duplicate && already_sent(&state_path, &idempotency_key);

    let mut approval_tx = None;
    if send_live && allowance_raw < U256::from(margin_scaled) {
        approval_tx = client.ensure_approval(U256::from(margin_scaled)).await?;
    }

    let calldata = contract
        .openMaker(ContractOpenMakerParams {
            holder: wallet_address,
            margin: margin_scaled,
            tickLower: i32_to_i24(tick_lower),
            tickUpper: i32_to_i24(tick_upper),
            liquidity,
            maxAmt0In: U256::MAX,
            maxAmt1In: U256::MAX,
        })
        .calldata()
        .clone();
    let sim_tx = TransactionRequest::default()
        .with_from(wallet_address)
        .with_to(perp_addr)
        .with_input(calldata);
    let simulation = client.provider().call(sim_tx).await;
    let simulation_ok = simulation.is_ok();
    let simulation_error = simulation.err().map(|e| {
        let mut s = e.to_string();
        if s.len() > 240 {
            s.truncate(240);
            s.push_str("...[truncated]");
        }
        s.replace('"', "'")
    });

    let mut maker_tx = None;
    if duplicate_blocked {
        eprintln!(
            "duplicate live liquidity run blocked; set ALLOW_DUPLICATE_LIQUIDITY=1 to override"
        );
    } else if send_live && simulation_ok {
        let result = client
            .open_maker(
                &OpenMakerParams {
                    margin,
                    price_lower,
                    price_upper,
                    liquidity,
                    max_amt0_in: u128::MAX,
                    max_amt1_in: u128::MAX,
                },
                Urgency::Normal,
            )
            .await?;
        record_sent(&state_path, &idempotency_key, result.tx_hash)?;
        maker_tx = Some(result.tx_hash);
    }

    println!(
        "{{\"market\":\"{market}\",\"perp\":\"{perp_addr}\",\"wallet\":\"{wallet_address}\",\"send_live\":{send_live},\"duplicate_blocked\":{duplicate_blocked},\"usdc_balance\":\"{}\",\"usdc_allowance\":\"{}\",\"margin_usdc\":{margin},\"mark\":{mark},\"index\":{index},\"direction\":\"{direction}\",\"price_lower\":{price_lower},\"price_upper\":{price_upper},\"tick_lower\":{tick_lower},\"tick_upper\":{tick_upper},\"liquidity\":\"{liquidity}\",\"simulation_ok\":{simulation_ok},\"simulation_error\":{},\"approval_tx\":{},\"maker_tx\":{}}}",
        as_usdc(balance_raw),
        as_usdc(allowance_raw),
        simulation_error
            .map(|e| format!("\"{e}\""))
            .unwrap_or_else(|| "null".to_string()),
        approval_tx
            .map(|h| format!("\"{h}\""))
            .unwrap_or_else(|| "null".to_string()),
        maker_tx
            .map(|h| format!("\"{h}\""))
            .unwrap_or_else(|| "null".to_string())
    );
    Ok(())
}
