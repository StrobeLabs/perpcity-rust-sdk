//! Sign PerpCity transactions with an AWS KMS asymmetric secp256k1 key.
//!
//! The private key lives in KMS and is never readable — every transaction is
//! signed remotely via the KMS `Sign` API. Access is controlled by IAM (e.g. a
//! task role attached to the bot), so rotating or revoking a bot's signing
//! rights is an IAM change, not a key redistribution.
//!
//! ```bash
//! # Requires the `aws` feature:
//! # AWS credentials come from the usual chain (env, SSO profile, task role).
//! export AWS_REGION="us-west-2"
//! export AWS_KMS_KEY_ID="arn:aws:kms:us-west-2:123456789012:key/..."  # or an alias/... name
//! export RPC_URL="https://sepolia-rollup.arbitrum.io/rpc"
//! export PERPCITY_PERP="0x..."
//! cargo run --release --features aws --example aws_kms_signer
//! ```

use alloy::primitives::{Address, U256};
use alloy::signers::aws::AwsSigner;
use perpcity_sdk::*;
use std::env;

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.into())
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let rpc_url = env_or("RPC_URL", "https://sepolia-rollup.arbitrum.io/rpc");

    // -- KMS signer --
    // Credentials resolve through the standard AWS chain: env vars, an SSO
    // profile, or the attached IAM role when running on ECS/EC2.
    let key_id = env::var("AWS_KMS_KEY_ID").expect("set AWS_KMS_KEY_ID");
    let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let kms = aws_sdk_kms::Client::new(&aws_config);
    let signer = AwsSigner::new(kms, key_id, Some(ARBITRUM_SEPOLIA_CHAIN_ID))
        .await
        .expect("failed to create AWS KMS signer (check credentials and key policy)");
    println!(
        "KMS signer address: {}",
        alloy::signers::Signer::address(&signer)
    );

    // -- Connect --
    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&rpc_url)
            .build()?,
    )?;

    let deployments = Deployments {
        perp: env::var("PERPCITY_PERP")
            .expect("set PERPCITY_PERP")
            .parse::<Address>()
            .unwrap(),
        usdc: ARBITRUM_SEPOLIA_USDC,
        pool_manager: ARBITRUM_SEPOLIA_POOL_MANAGER,
    };

    let client = PerpClient::new_arbitrum_sepolia(transport, signer, deployments)?;
    println!("connected to {rpc_url} as {}", client.address());

    // -- Warm caches --
    client.sync_nonce().await?;
    client.refresh_gas().await?;
    client.ensure_approval(U256::MAX).await?;

    // -- Read market state, then open/close a tiny position via KMS signing --
    let config = client.get_perp_config().await?;
    println!("mark price: {:.2}", config.mark);

    let open = client
        .open_taker(
            &OpenTakerParams {
                margin: 10.0,
                perp_delta: 1.0,
                amt1_limit: 0,
            },
            Urgency::Normal,
        )
        .await?;
    println!("opened position {} (signed via KMS)", open.pos_id);

    client.refresh_gas().await?;
    let result = client
        .adjust_taker(
            &AdjustTakerParams {
                pos_id: open.pos_id,
                margin_delta: 0.0,
                perp_delta: -1.0,
                amt1_limit: u128::MAX,
            },
            Urgency::Normal,
        )
        .await?;
    println!("closed: {}", result.tx_hash);

    Ok(())
}
