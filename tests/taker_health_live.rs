//! The claim the taker batch makes, checked against the chain's own
//! verdict: for every open taker on a live market, the port's
//! `is_liquidatable` at the lagged block agrees with `liquidateTaker`
//! simulated by `eth_call` at the head. The two blocks are a few apart,
//! so a position crossing the line between them reads as a disagreement
//! here rather than being hidden; rerun on one.
//!
//! Ignored by default: it reads every position a market ever minted and
//! probes each open taker over RPC.
//!
//! The market must run `v0.2.2-upgradeable`, the build the port follows.
//! A legacy market's pool key names no hook, and its `liquidateTaker` tests
//! after swapping the position closed, net of the swap and liquidation
//! fees, so a comparison there says nothing about the port.
//!
//! ```bash
//! RPC_URL=https://arb1.arbitrum.io/rpc \
//! PERPCITY_PERP=0xYOUR_V0_2_2_MARKET \
//! cargo test --test taker_health_live -- --ignored --nocapture
//! ```

use std::env;

use alloy::primitives::{Address, U256};
use perpcity_sdk::contracts::Perp;
use perpcity_sdk::{ChainReader, HftTransport, PerpCityError, TransactionError, TransportConfig};

/// The probe's sender and fee recipient. The deployed test decides health
/// before it moves a token, so an unfunded address sees the verdict; a
/// liquidation that would then charge the sender reverts past the test,
/// which this test reads as the contract's yes.
const PROBE: Address = Address::repeat_byte(0x01);

fn var(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.is_empty())
}

#[tokio::test]
#[ignore = "probes a live market over RPC; set RPC_URL and PERPCITY_PERP"]
async fn the_ports_verdict_is_the_contracts() {
    dotenvy::dotenv().ok();
    let rpc_url = var("RPC_URL").expect("set RPC_URL");
    let perp: Address = var("PERPCITY_PERP")
        .expect("set PERPCITY_PERP")
        .parse()
        .expect("a market address");

    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&rpc_url)
            .build()
            .unwrap(),
    )
    .unwrap();
    let chain = ChainReader::arbitrum(transport);
    let market = chain.market(perp);

    let state = market.state().await.unwrap();
    let next = state.next_pos_id().await.unwrap();
    let ids: Vec<U256> = (0..next).map(U256::from).collect();
    let outcomes = state.taker_healths(&ids).await.unwrap();
    println!("{} ids at block {}", ids.len(), state.block().number);

    let mut takers = 0usize;
    let mut failed = 0usize;
    let mut disagreements = Vec::new();
    for outcome in &outcomes {
        let health = match &outcome.row {
            Ok(Some(health)) => health,
            Ok(None) => continue,
            Err(e) => {
                failed += 1;
                println!("{} failed: {e}", outcome.pos_id);
                continue;
            }
        };
        takers += 1;
        let chain_says = match market
            .simulate_liquidate_taker(PROBE, outcome.pos_id, PROBE)
            .await
        {
            Ok(()) => true,
            Err(PerpCityError::Transaction(e)) if e.is_revert::<Perp::NotLiquidatable>() => false,
            Err(PerpCityError::Transaction(e)) if e.is_revert::<Perp::NonTakerPosition>() => {
                panic!(
                    "{} is a taker to the batch and not to the contract",
                    outcome.pos_id
                )
            }
            Err(PerpCityError::Transaction(TransactionError::SimulationReverted {
                selector,
                ..
            })) => {
                println!(
                    "{} reverted past the health check: {selector}",
                    outcome.pos_id
                );
                true
            }
            Err(e) => panic!("{} probe failed: {e}", outcome.pos_id),
        };
        println!(
            "{}: margin ratio {:.4}, distance {:?}, port {} chain {}",
            outcome.pos_id,
            health.margin_ratio(),
            health.liquidation_prices().distance(),
            health.is_liquidatable(),
            chain_says,
        );
        if health.is_liquidatable() != chain_says {
            disagreements.push(outcome.pos_id);
        }
    }
    println!("{takers} open takers, {failed} rows failed");
    assert_eq!(failed, 0, "every row read");
    assert!(
        disagreements.is_empty(),
        "the port and the contract disagree on {disagreements:?}"
    );
}
