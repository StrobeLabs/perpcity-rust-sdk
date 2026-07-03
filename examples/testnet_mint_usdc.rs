//! Mint Perp City TestnetUSDC to the local testnet bot wallet.
//!
//! Reads PERPCITY_TESTNET_BOT_PRIVATE_KEY from a chmod-600 env file.
//! Prints wallet address, balance summary, and tx hash only. Never prints key.

use std::{env, fs};

use alloy::primitives::{Address, U256, address};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use perpcity_sdk::{Deployments, HftTransport, PerpClient, TransportConfig};

const DEFAULT_KEY_FILE: &str = "/opt/data/secrets/perpcity/funding-arb-testnet.env";
const DEFAULT_RPC_URL: &str = "https://sepolia-rollup.arbitrum.io/rpc";
const TESTNET_USDC: Address = address!("BEF280BefeE2Cb28c20D1E4Cc1da999B4DA0f1fD");
const PLACEHOLDER_PERP: Address = address!("6d4051ffb71f391a5b4d8643a29Ec6F66F67df50");

sol! {
    #[sol(rpc)]
    interface ITestnetUSDC {
        function mint(address to, uint256 amount) external;
        function balanceOf(address account) external view returns (uint256);
    }
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

fn as_usdc(raw: U256) -> f64 {
    raw.to::<u128>() as f64 / 1_000_000.0
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key_file =
        env::var("PERPCITY_TESTNET_BOT_KEY_FILE").unwrap_or_else(|_| DEFAULT_KEY_FILE.to_string());
    let rpc_url = env::var("PERPCITY_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.to_string());
    let amount_usdc: u128 = env::var("MINT_TESTNET_USDC")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000);

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
            perp: PLACEHOLDER_PERP,
            usdc: TESTNET_USDC,
        },
    )?;
    client.sync_nonce().await?;
    client.refresh_gas().await?;

    let token = ITestnetUSDC::new(TESTNET_USDC, client.provider());
    let before = token.balanceOf(wallet_address).call().await?;
    let amount_raw = U256::from(amount_usdc * 1_000_000u128);
    let calldata = token.mint(wallet_address, amount_raw).calldata().clone();
    let receipt = client.tx(TESTNET_USDC, calldata).send().await?;
    let after = token.balanceOf(wallet_address).call().await?;

    println!(
        "{{\"wallet\":\"{wallet_address}\",\"mint_amount_usdc\":{amount_usdc},\"balance_before_usdc\":{},\"balance_after_usdc\":{},\"tx_hash\":\"{}\"}}",
        as_usdc(before),
        as_usdc(after),
        receipt.transaction_hash
    );
    Ok(())
}
