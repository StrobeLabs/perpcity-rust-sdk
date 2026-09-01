//! Preview maker settle equities for a batch of positions, then gate a
//! liquidation on the contract's own health check.
//!
//! ```bash
//! export RPC_URL="https://sepolia-rollup.arbitrum.io/rpc"
//! export PERPCITY_PRIVATE_KEY="0x..." # simulation msg.sender; no tx is sent
//! export PERPCITY_PERP="0x..."
//! export PERPCITY_POS_IDS="1,2,3"     # position ids to preview
//! cargo run --release --example maker_equity
//! ```

use std::env;

use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;
use perpcity_sdk::{
    ARBITRUM_SEPOLIA_POOL_MANAGER, ARBITRUM_SEPOLIA_USDC, Deployments, HftTransport,
    MakerEquityKind, PerpCityError, PerpClient, TransactionError, TransportConfig, Urgency,
};

#[tokio::main]
async fn main() -> perpcity_sdk::Result<()> {
    dotenvy::dotenv().ok();
    let rpc_url = env::var("RPC_URL").expect("set RPC_URL");
    let signer: PrivateKeySigner = env::var("PERPCITY_PRIVATE_KEY")
        .expect("set PERPCITY_PRIVATE_KEY")
        .parse()
        .expect("invalid private key");
    let perp = address("PERPCITY_PERP");
    let pos_ids: Vec<U256> = env::var("PERPCITY_POS_IDS")
        .unwrap_or_else(|_| "1,2,3".into())
        .split(',')
        .map(|id| id.trim().parse().expect("invalid position id"))
        .collect();

    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&rpc_url)
            .build()?,
    )?;
    let client = PerpClient::new_arbitrum_sepolia(
        transport,
        signer,
        Deployments {
            perp,
            usdc: ARBITRUM_SEPOLIA_USDC,
            pool_manager: ARBITRUM_SEPOLIA_POOL_MANAGER,
        },
    )?;

    // One batched, block-pinned read: the settle preview each open maker in
    // `pos_ids` would receive if it were touched now. Taker/burned ids are
    // omitted; a single bad position degrades alone.
    let equities = client.get_maker_equities(&pos_ids).await?;
    for outcome in &equities {
        let pos_id = outcome.pos_id;
        match &outcome.kind {
            MakerEquityKind::Computed(b) => println!(
                "pos {pos_id}: equity={:+.6} settled_margin={:+.6} \
                 funding={:+.6} util={:+.6} lp_fees={:+.6} pnl={:+.6}",
                b.equity(),
                b.settled_margin(),
                b.funding_owed_usd(),
                b.long_util_earnings_usd() + b.short_util_earnings_usd(),
                b.lp_fees_usd(),
                b.unrealized_pnl_usd(),
            ),
            MakerEquityKind::NotAMaker => println!("pos {pos_id}: not an open maker"),
            MakerEquityKind::Failed(e) => println!("pos {pos_id}: read failed: {e}"),
        }
    }

    // Liquidations are gated on the contract's own health check: the
    // eth_call runs from this client's address at the same pinned gas limit
    // as the send, and a typed revert says exactly why not — retry
    // NotLiquidatable later, drop NonMakerPosition, keep going on
    // transients.
    let fee_recipient = client.address();
    for outcome in &equities {
        let pos_id = outcome.pos_id;
        match client.simulate_liquidate_maker(pos_id, fee_recipient).await {
            Ok(()) => {
                println!("pos {pos_id}: LIQUIDATABLE — sending");
                let receipt = client
                    .liquidate_maker(pos_id, fee_recipient, Urgency::Critical)
                    .await?;
                println!("pos {pos_id}: liquidated in {}", receipt.transaction_hash);
            }
            Err(PerpCityError::Transaction(TransactionError::SimulationReverted {
                error_name,
                ..
            })) => println!("pos {pos_id}: not liquidatable ({error_name})"),
            Err(e) if e.is_transient() => println!("pos {pos_id}: transient ({e}); retry"),
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

fn address(name: &str) -> Address {
    env::var(name)
        .unwrap_or_else(|_| panic!("set {name}"))
        .parse()
        .unwrap_or_else(|_| panic!("invalid {name} address"))
}
