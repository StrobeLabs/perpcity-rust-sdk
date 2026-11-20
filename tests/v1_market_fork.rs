//! Market reads on the W11 fork and W12 vectors from the partner plan.
mod v1_fork;

use alloy::primitives::{B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol_types::SolError;
use perpcity_sdk::constants::{V1_MAX_INDEX_X96, V1_MIN_INDEX_X96};
use perpcity_sdk::contracts::v1::{IFees, IPriceImpact, IPricing, Perp, PricePair as ModulePair};
use perpcity_sdk::math::pricing::{PricePair, calculate_emas};
use perpcity_sdk::{
    ChainReader, HftTransport, MarketCapacity, PerSide, PerpAtoms, Side, TransportConfig,
};
use serde_json::Value;
use v1_fork::{FEES, PRICE_IMPACT, PRICING, SepoliaFork, V1Fork, fork_url};

fn vectors() -> Vec<Value> {
    serde_json::from_str(include_str!("fixtures/w12-market-reads.json")).unwrap()
}

fn word(row: &Value, key: &str) -> U256 {
    row[key].as_str().unwrap().parse().unwrap()
}

fn advanced(case: &Value) -> PricePair {
    let i = &case["inputs"];
    let step = case["kind"] == "ema_step";
    let (amm, index, dt) = if step {
        ("prev_ema_amm_x96", "prev_ema_index_x96", word(i, "dt_s"))
    } else {
        (
            "stored_ema_amm_x96",
            "stored_ema_index_x96",
            word(i, "now") - word(i, "last_touch"),
        )
    };
    let sqrt = word(i, "sqrt_price_x96");
    calculate_emas(
        PricePair {
            amm: word(i, amm).to(),
            index: word(i, index).to(),
        },
        PricePair {
            amm: ((sqrt * sqrt) >> 96usize).to(),
            index: word(i, "index_x96").to(),
        },
        0,
        dt.to(),
        word(i, "ema_window_s").to(),
    )
    .unwrap()
}

#[test]
fn w12_ema_steps_and_advanced_mark_inputs() {
    for case in vectors() {
        let expected = match case["kind"].as_str().unwrap() {
            "ema_step" => &case["expected"],
            "mark_from_state" => &case["intermediate"],
            _ => continue,
        };
        assert_eq!(
            advanced(&case),
            PricePair {
                amm: word(expected, "ema_amm_x96").to(),
                index: word(expected, "ema_index_x96").to()
            },
            "{}",
            case["id"]
        );
    }
}

#[tokio::test]
#[ignore = "reads the deployed Arbitrum Sepolia modules"]
async fn w12_module_answers() {
    let provider = ProviderBuilder::new().connect_http(fork_url().parse().unwrap());
    let pricing = IPricing::new(PRICING, &provider);
    let impact = IPriceImpact::new(PRICE_IMPACT, &provider);
    let fees = IFees::new(FEES, &provider);
    let block = provider.get_block_number().await.unwrap().into();
    for c in vectors() {
        let i = &c["inputs"];
        let e = &c["expected"];
        let n = |key| word(i, key);
        let expected = |key| word(e, key);
        match c["kind"].as_str().unwrap() {
            "ema_step" => continue,
            "fair_price" | "mark_from_state" => {
                let index = n("index_x96");
                let (pool, a, b) = if c["kind"] == "fair_price" {
                    (index, n("ema_amm_x96"), n("ema_index_x96"))
                } else {
                    let pair = advanced(&c);
                    let sqrt = n("sqrt_price_x96");
                    (
                        (sqrt * sqrt) >> 96usize,
                        U256::from(pair.amm),
                        U256::from(pair.index),
                    )
                };
                assert_eq!(
                    pricing
                        .fairPrice(pool, index, a, b)
                        .block(block)
                        .call()
                        .await
                        .unwrap(),
                    expected("mark_x96"),
                    "{}",
                    c["id"]
                );
            }
            "fees_module_rates" => {
                let pair = |key| ModulePair {
                    ammPrice: n(key).to(),
                    index: n(key).to(),
                };
                let r = fees
                    .fees(
                        n("liquidity").to(),
                        pair("spot_amm_price_x96"),
                        pair("ema_amm_price_x96"),
                    )
                    .block(block)
                    .call()
                    .await
                    .unwrap();
                assert_eq!(
                    (r.cFee, r.insFee, r.lpFee),
                    (
                        expected("creator_fee_e6").to(),
                        expected("insurance_fee_e6").to(),
                        expected("lp_fee_e6").to()
                    ),
                    "{}",
                    c["id"]
                );
            }
            "max_swap_fee" => assert_eq!(
                fees.maxSwapFee().block(block).call().await.unwrap(),
                expected("max_swap_fee_e6")
            ),
            "price_impact_bounds" => {
                let m = n("mark_x96");
                let r = impact
                    .sqrtPriceBounds(m, m, m, m, m)
                    .block(block)
                    .call()
                    .await
                    .unwrap();
                assert_eq!(
                    (r.sqrtMin, r.sqrtMax),
                    (expected("sqrt_min_x96"), expected("sqrt_max_x96")),
                    "{}",
                    c["id"]
                );
            }
            "util_fee_rate" | "util_fee_rates_after_refresh" => {
                let (long, short, l, s) = if c["kind"] == "util_fee_rate" {
                    (
                        n("long_utilization_e6"),
                        n("short_utilization_e6"),
                        "long_fee_per_day",
                        "short_fee_per_day",
                    )
                } else {
                    let sides =
                        |l, s| PerSide::new(PerpAtoms::new(n(l).to()), PerpAtoms::new(n(s).to()));
                    let cap = MarketCapacity {
                        capacity: sides("cap_long", "cap_short"),
                        open_interest: sides("oi_long", "oi_short"),
                        close_capacity: Some(Default::default()),
                        ..Default::default()
                    };
                    let util = |side| U256::from(cap.utilization_e6(side).unwrap());
                    (
                        util(Side::Long),
                        util(Side::Short),
                        "long_util_fee_per_day",
                        "short_util_fee_per_day",
                    )
                };
                let r = fees
                    .utilFees(long, short)
                    .block(block)
                    .call()
                    .await
                    .unwrap();
                assert_eq!(
                    (r.longFee, r.shortFee),
                    (expected(l).to(), expected(s).to()),
                    "{}",
                    c["id"]
                );
            }
            other => panic!("unknown W12 vector {other}"),
        }
    }
}

#[tokio::test]
#[ignore = "requires Anvil and Arbitrum Sepolia"]
async fn market_reads_match_raw_views() {
    let fork = SepoliaFork::start().await;
    let mut w = V1Fork::new(&fork);
    let m = w.build_w11_market().await;
    let _: Value = w
        .provider
        .raw_request("anvil_mine".into(), ("0x9",))
        .await
        .unwrap();
    let chain = ChainReader::arbitrum_sepolia(
        HftTransport::new(
            TransportConfig::builder()
                .shared_endpoint(&fork.url)
                .build()
                .unwrap(),
        )
        .unwrap(),
    )
    .checked()
    .await
    .unwrap();
    let market = chain.market(m.perp);
    let config = market.get_config().await.unwrap();
    let (snap_config, snap) = market.get_snapshot().await.unwrap();
    let raw = Perp::new(m.perp, &w.provider);
    assert_eq!(
        config.ema_window,
        raw.emaWindow().call().await.unwrap().to::<u64>()
    );
    let block = snap.block.number.into();
    assert_eq!(
        snap_config.ema_window,
        raw.emaWindow()
            .block(block)
            .call()
            .await
            .unwrap()
            .to::<u64>()
    );
    let state = market.state_at(snap.block.number).await.unwrap();
    let mark = raw
        .previewPosition(m.maker_id)
        .block(block)
        .call()
        .await
        .unwrap()
        .markPrice;
    assert_eq!(snap.mark.x96(), mark);
    assert_eq!(state.mark().await.unwrap().price.x96(), mark);
    let cap = state.capacity().await.unwrap();
    let band = raw.capacity().block(block).call().await.unwrap();
    let close = raw
        .physicalCloseCapacity()
        .block(block)
        .call()
        .await
        .unwrap();
    assert_eq!(
        (cap.capacity.long.atoms(), cap.capacity.short.atoms()),
        (band.long, band.short)
    );
    let depth = cap.close_capacity.unwrap();
    assert_eq!(state.physical_close_capacity().await.unwrap(), depth);
    assert_eq!(
        (depth.long.atoms(), depth.short.atoms()),
        (close.longClose, close.shortClose)
    );
    // Change only cFee in the first packed word of the local Fees instance.
    let old_fee = config.fees.creator_fee.e6();
    let new_fee = old_fee + 1;
    let packed = w.provider.get_storage_at(FEES, U256::ZERO).await.unwrap();
    assert_eq!((packed & U256::from(0xffffff)).to::<u32>(), old_fee);
    let _: Value = w.provider.raw_request("evm_mine".into(), ()).await.unwrap();
    let _: Value = w
        .provider
        .raw_request(
            "anvil_setStorageAt".into(),
            (
                FEES,
                B256::ZERO,
                B256::from(packed - U256::from(old_fee) + U256::from(new_fee)),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        market.get_config().await.unwrap().fees.creator_fee.e6(),
        new_fee
    );
    assert_eq!(
        market.get_snapshot().await.unwrap().0.fees.creator_fee.e6(),
        old_fee
    );
    let _: Value = w
        .provider
        .raw_request("anvil_mine".into(), ("0x9",))
        .await
        .unwrap();
    assert_eq!(
        market.get_snapshot().await.unwrap().0.fees.creator_fee.e6(),
        new_fee
    );

    // Return controlled index observations from the local beacon. The real
    // Perp preview and the SDK then consume the same words at the same block.
    let latency = raw.latency().call().await.unwrap();
    let observations = [
        (V1_MIN_INDEX_X96, i64::from(latency), None),
        (V1_MAX_INDEX_X96, 0, None),
        (
            mark,
            i64::from(latency) + 1,
            Some(Perp::StaleBeaconIndex::SELECTOR),
        ),
        (U256::ZERO, -1, Some(Perp::StaleBeaconIndex::SELECTOR)),
        (
            V1_MIN_INDEX_X96 - U256::ONE,
            0,
            Some(Perp::PriceOutsideRange::SELECTOR),
        ),
        (
            V1_MAX_INDEX_X96 + U256::ONE,
            0,
            Some(Perp::PriceOutsideRange::SELECTOR),
        ),
    ];
    let cases = observations
        .into_iter()
        .map(|(price, age, refusal)| (Some(price), age, refusal, ""))
        .chain(
            ["0x60006000fd", "0x603f6000f3"]
                .map(|code| (None, 0, Some(Perp::BeaconIndexUnreadable::SELECTOR), code)),
        );
    for (price, age, refusal, code) in cases {
        let timestamp = w.now().await + 100;
        let updated = timestamp.checked_add_signed(-age).unwrap();
        // PUSH32 price, MSTORE(0), PUSH32 updated, MSTORE(32), RETURN(0,64).
        let code = price.map_or_else(
            || code.to_owned(),
            // CALLER must equal the market, else revert. JUMPDEST is at byte 31.
            |price| format!("0x3373{:040x}14601f5760006000fd5b7f{price:064x}6000527f{updated:064x}60205260406000f3", m.perp),
        );
        let _: Value = w
            .provider
            .raw_request("anvil_setCode".into(), (m.beacon, code))
            .await
            .unwrap();
        let _: Value = w
            .provider
            .raw_request("evm_setNextBlockTimestamp".into(), (timestamp,))
            .await
            .unwrap();
        let _: Value = w.provider.raw_request("evm_mine".into(), ()).await.unwrap();
        let height = w.provider.get_block_number().await.unwrap();
        let state = market.state_at(height).await.unwrap();
        assert_eq!(state.block().timestamp, timestamp);
        let sdk = state.mark().await;
        let preview = raw
            .previewPosition(m.maker_id)
            .block(height.into())
            .call()
            .await;
        if let Some(selector) = refusal {
            let Err(err) = preview else {
                panic!("preview accepted the observation")
            };
            assert_eq!(&err.as_revert_data().unwrap()[..4], selector);
            assert!(
                matches!(sdk.unwrap_err(), perpcity_sdk::PerpCityError::Transaction(perpcity_sdk::TransactionError::SimulationReverted { selector: actual, .. }) if actual.0 == selector)
            );
        } else {
            assert_eq!(sdk.unwrap().price.x96(), preview.unwrap().markPrice);
        }
    }
}
