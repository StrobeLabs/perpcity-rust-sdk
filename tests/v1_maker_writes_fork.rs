//! SDK maker writes on an Anvil fork of Arbitrum Sepolia.
//! Run with `cargo test --test v1_maker_writes_fork -- --ignored --nocapture`.

mod v1_fork;

use std::time::{SystemTime, UNIX_EPOCH};

use alloy::consensus::Transaction as _;
use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::providers::Provider;
use alloy::sol_types::{SolCall, SolError};
use perpcity_sdk::contracts::v1::{AdjustMakerParams, Perp};
use perpcity_sdk::events::receipt::MakerExit;
use perpcity_sdk::hft::gas::GasLimitCache;
use perpcity_sdk::{
    ChainReader, ContractError, Era, ExactAdjustMakerParams, ExactOpenMakerParams, HftTransport,
    LDelta, LUnits, PerpAtoms, PerpCityError, PerpClient, TickRange, TransportConfig, Urgency,
    UsdcAtoms, UsdcDelta, V1MakerAdjustment, V1MakerReplacement, ValidationError,
};
use serde_json::Value;

use v1_fork::{SepoliaFork, V1Fork};

fn wall_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn open_params(lower: i32, upper: i32, liquidity: u128) -> ExactOpenMakerParams {
    ExactOpenMakerParams {
        range: TickRange::new(lower, upper).unwrap(),
        liquidity: LUnits::new(liquidity),
        margin: UsdcAtoms::new(v1_fork::MAKER_MARGIN),
        max_amt0_in: PerpAtoms::new(u128::MAX),
        max_amt1_in: UsdcAtoms::new(u128::MAX),
    }
}

async fn observe(w: &V1Fork, hash: B256, label: &str) -> Bytes {
    let receipt = w
        .provider
        .get_transaction_receipt(hash)
        .await
        .unwrap()
        .unwrap();
    println!(
        "{label}: block {} gas {} tx {hash}",
        receipt.block_number.unwrap(),
        receipt.gas_used
    );
    let calldata = w
        .provider
        .get_transaction_by_hash(hash)
        .await
        .unwrap()
        .unwrap()
        .input()
        .clone();
    let selector = calldata[..4].try_into().unwrap();
    let mut limits = GasLimitCache::new();
    limits.put(selector, 1, 0);
    assert!(
        limits.get(&selector, 0).unwrap() >= (receipt.gas_used * 6).div_ceil(5),
        "{label}"
    );
    calldata
}

#[tokio::test]
#[ignore] // Requires Anvil and an Arbitrum Sepolia endpoint.
async fn v1_maker_writes_on_a_sepolia_fork() {
    let fork = SepoliaFork::start().await;
    let mut w = V1Fork::new(&fork);
    let residual_market = w.build_w11_market().await;
    let moved = w
        .steps
        .iter()
        .find(|step| step.label == "move-maker")
        .unwrap();
    observe(&w, moved.receipt.transaction_hash, "maker-move").await;
    // Use the same index as both W12 maker amount vectors.
    w.push_index(
        "restore-w12-index",
        residual_market.verifier,
        residual_market.beacon,
        v1_fork::index_1(),
    )
    .await;
    let perp = w.create_perp(residual_market.beacon).await;
    w.approve_usdc(perp).await;

    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&fork.url)
            .build()
            .unwrap(),
    )
    .unwrap();
    let chain = ChainReader::arbitrum_sepolia(transport)
        .checked()
        .await
        .unwrap();
    let client = PerpClient::new(chain.market(perp), w.signer.clone());
    client.sync_nonce().await.unwrap();
    assert_eq!(client.market().era().await.unwrap(), Era::V1);
    chain.refresh_gas().await.unwrap();

    let cases: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/w12-maker-open-amounts.json")).unwrap();
    assert_eq!(cases.len(), 2);
    for case in cases {
        let input = |key: &str| case["inputs"][key].as_str().unwrap();
        let expected = |key: &str| {
            case["expected"][key]
                .as_str()
                .unwrap()
                .parse::<u128>()
                .unwrap()
        };
        let mut params = open_params(
            input("tick_lower").parse().unwrap(),
            input("tick_upper").parse().unwrap(),
            input("liquidity").parse().unwrap(),
        );
        params.max_amt0_in = PerpAtoms::new(expected("amount0_in"));
        params.max_amt1_in = UsdcAtoms::new(expected("amount1_in"));
        let before = wall_secs();
        chain.refresh_gas().await.unwrap();
        let opened = client
            .open_maker_v1(&params, Urgency::Normal)
            .await
            .unwrap();
        let after = wall_secs();
        let input = observe(&w, opened.tx_hash, case["id"].as_str().unwrap()).await;
        let sent = Perp::openMakerCall::abi_decode(&input).unwrap().params;
        assert!((before + 600..=after + 600).contains(&sent.deadline));
        assert_eq!(sent.holder, w.me);
        assert_eq!(opened.event.owner, w.me);
        assert_eq!(opened.event.liquidity, params.liquidity.units());
        let stored = client.market().get_position(opened.pos_id).await.unwrap();
        assert_eq!(stored.perp.atoms(), -(params.max_amt0_in.atoms() as i128));
        assert_eq!(stored.usd.atoms(), -(params.max_amt1_in.atoms() as i128));
    }

    // Below the pool price this band holds only USDC, so full removal
    // emits MakerClosed. The W12 bands above also let us test conversion.
    let params = open_params(38_400, 38_700, 200_000_000);
    chain.refresh_gas().await.unwrap();
    let opened = client
        .open_maker_v1(&params, Urgency::Normal)
        .await
        .unwrap();
    observe(&w, opened.tx_hash, "maker-open-below-price").await;
    let adjust = ExactAdjustMakerParams {
        pos_id: opened.pos_id,
        margin_delta: UsdcDelta::ZERO,
        liquidity_delta: LDelta::new(-50_000_000),
        amt0_limit: PerpAtoms::ZERO,
        amt1_limit: UsdcAtoms::ZERO,
    };
    let before = wall_secs();
    chain.refresh_gas().await.unwrap();
    let V1MakerAdjustment::Adjusted(adjusted) = client
        .adjust_maker_v1(&adjust, Urgency::Normal)
        .await
        .unwrap()
    else {
        panic!("partial removal must emit MakerAdjusted");
    };
    let after = wall_secs();
    let input = observe(&w, adjusted.tx_hash, "maker-adjust-partial-removal").await;
    let sent = Perp::adjustMakerCall::abi_decode(&input).unwrap().params;
    assert!((before + 600..=after + 600).contains(&sent.deadline));
    assert_eq!(sent.liquidityDelta, -50_000_000);
    assert_eq!(adjusted.event.liquidity, 150_000_000);

    chain.refresh_gas().await.unwrap();
    let before = wall_secs();
    let MakerExit::Closed(closed) = client
        .close_maker_v1(opened.pos_id, Urgency::Normal)
        .await
        .unwrap()
    else {
        panic!("the below-price band must close");
    };
    let after = wall_secs();
    let input = observe(&w, closed.tx_hash, "maker-close").await;
    let sent = Perp::adjustMakerCall::abi_decode(&input).unwrap().params;
    assert!((before + 600..=after + 600).contains(&sent.deadline));
    assert_eq!(sent.liquidityDelta, -150_000_000);
    let raw = Perp::new(perp, &w.provider);
    assert_eq!(
        raw.positionOwner(closed.pos_id).call().await.unwrap(),
        Address::ZERO
    );

    let V1MakerReplacement::Adjustment(V1MakerAdjustment::Exit(MakerExit::Closed(original))) =
        client
            .replace_v1_maker(closed.tx_hash, Urgency::High)
            .await
            .unwrap()
    else {
        panic!("the original maker close already mined");
    };
    assert_eq!(original.tx_hash, closed.tx_hash);

    chain.refresh_gas().await.unwrap();
    let MakerExit::Converted(converted) = client
        .close_maker_v1(U256::ONE, Urgency::Normal)
        .await
        .unwrap()
    else {
        panic!("rounding leaves a residual short in the first W12 band");
    };
    observe(&w, converted.tx_hash, "maker-convert").await;
    assert_eq!(converted.event.liquidity, 0);
    assert_eq!(
        raw.positionOwner(converted.pos_id).call().await.unwrap(),
        w.me
    );

    let residual = PerpClient::new(chain.market(residual_market.perp), w.signer.clone());
    let raw = Perp::new(residual_market.perp, &w.provider);
    let id = residual_market.maker_id;
    let state = residual.market().state_at_head().await.unwrap();
    let band = state.maker_band(id).await.unwrap().unwrap();
    assert_eq!(
        state.maker_liquidity().await.unwrap(),
        U256::from(band.liquidity.units())
    );
    let interest = state.capacity().await.unwrap().open_interest;
    assert_eq!(interest.long, PerpAtoms::ZERO);
    assert_eq!(interest.short, PerpAtoms::ZERO);
    let nonce = w.provider.get_transaction_count(w.me).await.unwrap();

    let error = residual
        .close_maker_v1(id, Urgency::Normal)
        .await
        .unwrap_err();
    assert!(
        matches!(error, PerpCityError::Validation(ValidationError::ShortCloseCapacityExceeded { interest, last_maker: true }) if interest > 0),
        "{error}"
    );
    let error = residual
        .adjust_maker_v1(
            &ExactAdjustMakerParams {
                pos_id: id,
                liquidity_delta: band.liquidity.negated().unwrap(),
                ..adjust
            },
            Urgency::Normal,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            PerpCityError::Validation(ValidationError::ShortCloseCapacityExceeded {
                last_maker: true,
                ..
            })
        ),
        "{error}"
    );

    let revert = raw
        .adjustMaker(AdjustMakerParams {
            posId: id,
            marginDelta: 0,
            liquidityDelta: band.liquidity.negated().unwrap().units(),
            amt0Limit: U256::ZERO,
            amt1Limit: U256::ZERO,
            deadline: w.deadline().await,
        })
        .from(w.me)
        .call()
        .await
        .err()
        .expect("the chain refuses the removal");
    let data = revert.as_revert_data().unwrap();
    let f0: Value =
        serde_json::from_str(include_str!("../src/errors/fixtures/v1-reverts.json")).unwrap();
    let expected = f0["reverts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "ShortCloseCapacityExceeded")
        .unwrap();
    assert_eq!(
        data,
        expected["revert_data"]
            .as_str()
            .unwrap()
            .parse::<Bytes>()
            .unwrap()
    );
    assert_eq!(data.as_ref(), Perp::ShortCloseCapacityExceeded::SELECTOR);
    println!("last maker with short residual: {data} equals F0");
    let err = residual
        .transfer_position(Address::with_last_byte(1), id, Urgency::Normal)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            PerpCityError::Contract(ContractError::UnsupportedEra {
                era: Era::V1,
                operation: "transfer_position"
            })
        ),
        "{err}"
    );
    assert_eq!(w.provider.get_transaction_count(w.me).await.unwrap(), nonce);

    // Backstops use raw calls to measure the selectors used by the gas cache.
    // A fresh provider reads the nonce after the SDK writes.
    let mut w = V1Fork::new(&fork);
    let raw = Perp::new(perp, w.provider.clone());
    w.open_maker(perp).await;
    let taker = w.open_taker(perp).await;
    w.push_index(
        "lower-index-for-taker-backstop",
        residual_market.verifier,
        residual_market.beacon,
        U256::from(20u64) << 96,
    )
    .await;
    let recipient = Address::with_last_byte(1);
    let hash = w
        .send(
            "taker-backstop",
            raw.backstopTaker(taker, 20_000_000, recipient, w.deadline().await),
        )
        .await
        .transaction_hash;
    observe(&w, hash, "taker-backstop").await;
    assert_eq!(raw.positionOwner(taker).call().await.unwrap(), recipient);

    w.push_index(
        "raise-index-for-maker-backstop",
        residual_market.verifier,
        residual_market.beacon,
        U256::from(200u64) << 96,
    )
    .await;
    let maker = U256::from(2u8);
    let hash = w
        .send(
            "maker-backstop",
            raw.backstopMaker(maker, 200_000_000, recipient, w.deadline().await),
        )
        .await
        .transaction_hash;
    observe(&w, hash, "maker-backstop").await;
    assert_eq!(raw.positionOwner(maker).call().await.unwrap(), recipient);
}
