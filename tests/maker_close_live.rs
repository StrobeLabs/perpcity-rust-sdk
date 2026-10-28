//! The claim a full close makes, checked against the chain's own verdict:
//! for every open maker on a live market, `MakerEquityBreakdown::full_close`
//! refuses exactly when `adjustMaker` removing all of its liquidity, called
//! from its owner by `eth_call` at the same block, reverts, and with the
//! same error.
//!
//! Ignored by default: it reads every position a market ever minted and
//! calls the close for each open maker over RPC.
//!
//! The market must run `v0.2.2-upgradeable`, the build the port follows.
//!
//! ```bash
//! RPC_URL=https://arb1.arbitrum.io/rpc \
//! PERPCITY_PERP=0xYOUR_V0_2_2_MARKET \
//! cargo test --test maker_close_live -- --ignored --nocapture
//! ```

use std::env;

use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use alloy::sol_types::SolError;
use perpcity_sdk::contracts::{AdjustMakerParams, Perp};
use perpcity_sdk::{
    ChainReader, CloseRefusal, HftTransport, MakerEquityKind, Remainder, TransportConfig,
};

fn var(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.is_empty())
}

/// The refusal a revert's selector names, or `None` for one a full close
/// does not predict.
fn refusal(selector: &[u8]) -> Option<CloseRefusal> {
    [
        (
            Perp::LongUtilizationExceeded::SELECTOR,
            CloseRefusal::LongUtilizationExceeded,
        ),
        (
            Perp::ShortUtilizationExceeded::SELECTOR,
            CloseRefusal::ShortUtilizationExceeded,
        ),
        (
            Perp::MarginRatioTooLow::SELECTOR,
            CloseRefusal::MarginRatioTooLow,
        ),
        (Perp::NegativeMargin::SELECTOR, CloseRefusal::NegativeMargin),
    ]
    .into_iter()
    .find(|(known, _)| selector.starts_with(known))
    .map(|(_, refusal)| refusal)
}

#[tokio::test]
#[ignore = "calls a live market over RPC; set RPC_URL and PERPCITY_PERP"]
async fn the_close_refuses_as_the_contract_does() {
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
    let state = chain.market(perp).state().await.unwrap();
    let block = BlockId::number(state.block().number);
    let ids: Vec<U256> = (0..state.next_pos_id().await.unwrap())
        .map(U256::from)
        .collect();
    let market = state.capacity().await.unwrap();
    let taker_liquidation = state.margin_ratios().await.unwrap().taker.liquidation;
    let owners = state.owners(&ids).await;
    let contract = Perp::new(perp, chain.provider());
    println!("{} ids at block {}", ids.len(), state.block().number);

    let mut makers = 0usize;
    let mut disagreements = Vec::new();
    for outcome in state.maker_equities(&ids).await.unwrap() {
        let breakdown = match &outcome.kind {
            MakerEquityKind::Computed(breakdown) => breakdown,
            MakerEquityKind::NotAMaker => continue,
            MakerEquityKind::Failed(e) => panic!("{} failed: {e}", outcome.pos_id),
        };
        makers += 1;
        let close = breakdown.full_close(&market, taker_liquidation).unwrap();
        let owner = owners
            .iter()
            .find(|o| o.pos_id == outcome.pos_id)
            .and_then(|o| o.row.as_ref().ok().copied().flatten())
            .expect("an open maker has an owner");
        let band = state
            .maker_band(outcome.pos_id)
            .await
            .unwrap()
            .expect("an open maker has a band");
        let chain_says = match contract
            .adjustMaker(AdjustMakerParams {
                posId: outcome.pos_id,
                marginDelta: 0,
                liquidityDelta: -i128::try_from(band.liquidity.units()).unwrap(),
                amt0Limit: U256::ZERO,
                amt1Limit: U256::ZERO,
            })
            .from(owner)
            .block(block)
            .call()
            .await
        {
            Ok(_) => None,
            Err(e) => {
                let data = e.as_revert_data().expect("a revert");
                Some(refusal(&data).unwrap_or_else(|| {
                    panic!("{} reverted with an unpredicted {data}", outcome.pos_id)
                }))
            }
        };
        let left = match close.remainder() {
            Remainder::Payout(payout) => format!("payout {:.6}", payout.usdc()),
            Remainder::Taker(taker) => format!(
                "taker {} perp atoms, margin ratio {:.4}",
                taker.delta_perp().atoms(),
                taker.margin_ratio()
            ),
        };
        println!(
            "{}: {left}, port {:?} chain {:?}",
            outcome.pos_id,
            close.refusal(),
            chain_says
        );
        if close.refusal() != chain_says {
            disagreements.push(outcome.pos_id);
        }
    }
    println!("{makers} open makers");
    assert!(
        disagreements.is_empty(),
        "the port and the contract disagree on {disagreements:?}"
    );
}
