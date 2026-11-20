//! SDK maker writes on an Anvil fork of Arbitrum Sepolia.
//! Run with `cargo test --test v1_maker_writes_fork -- --ignored --nocapture`.

mod v1_fork;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::consensus::Transaction as _;
use alloy::contract::{CallBuilder, CallDecoder};
use alloy::primitives::aliases::U48;
use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::providers::Provider;
use alloy::sol_types::{SolCall, SolError};
use perpcity_sdk::contracts::v1::{
    AdjustMakerParams, MoveMakerParams, OpenMakerParams, Perp, WithdrawalConfig,
};
use perpcity_sdk::events::receipt::MakerExit;
use perpcity_sdk::math::liquidity::band_amounts;
use perpcity_sdk::{
    ChainReader, ContractError, Era, ExactAdjustMakerParams, ExactOpenMakerParams, HftTransport,
    LDelta, LUnits, MakerBand, PerSide, PerpAtoms, PerpCityError, PerpClient, TickRange,
    TransactionError, TransportConfig, Urgency, UsdcAtoms, UsdcDelta, V1MakerAdjustment,
    V1MakerReplacement, ValidationError, band_capacity_x64,
};
use serde_json::Value;
use tokio::time::sleep;
use tracing::instrument::WithSubscriber;

use v1_fork::{SepoliaFork, V1Fork};

/// The quiet seconds before the wide move, so it accrues over a long gap.
const QUIET: u64 = 300;
/// The range the W11 maker moves to, and the margin it adds.
const WIDE: (i32, i32) = (33_000, 45_000);
const WIDE_MARGIN: i128 = 500_000_000;

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

/// The mined call's calldata, after its gas is checked against its
/// selector's floor.
async fn observe(w: &V1Fork, hash: B256, label: &str) -> Bytes {
    v1_fork::measure_gas(&w.provider, hash, label).await;
    w.provider
        .get_transaction_by_hash(hash)
        .await
        .unwrap()
        .unwrap()
        .input()
        .clone()
}

/// The selector `call` reverts with from the dev wallet at the head.
async fn revert_selector<P: Provider, D: CallDecoder>(
    w: &V1Fork,
    call: CallBuilder<P, D>,
) -> [u8; 4] {
    let error = call
        .from(w.me)
        .call()
        .await
        .err()
        .expect("the call reverts");
    let data = error.as_revert_data().expect("a revert with data");
    data[..4].try_into().unwrap()
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
    // ── The W11 maker moved wide after a quiet gap, sent by the SDK ──
    w.mint_usdc().await;
    w.mint_usdc().await;
    w.warp(QUIET).await;
    let _: Value = w.provider.raw_request("evm_mine".into(), ()).await.unwrap();
    let wide = PerpClient::new(chain.market(residual_market.perp), w.signer.clone());
    wide.sync_nonce().await.unwrap();
    chain.refresh_gas().await.unwrap();
    let move_wide = Perp::moveMakerCall {
        params: MoveMakerParams {
            posId: residual_market.maker_id,
            tickLower: WIDE.0.try_into().unwrap(),
            tickUpper: WIDE.1.try_into().unwrap(),
            liquidity: v1_fork::MOVE_LIQUIDITY - v1_fork::REDUCE_LIQUIDITY,
            removeAmt0Min: U256::ZERO,
            removeAmt1Min: U256::ZERO,
            addAmt0Max: U256::MAX,
            addAmt1Max: U256::MAX,
            marginDelta: WIDE_MARGIN,
            deadline: w.deadline().await,
        },
    };
    let moved = wide
        .tx(residual_market.perp, move_wide.abi_encode().into())
        .send()
        .await
        .unwrap();
    let gas = v1_fork::measure_gas(&w.provider, moved.transaction_hash, "maker-move-wide").await;
    println!(
        "wide move after {QUIET} s: used {}, sent limit {}",
        gas.used, gas.sent
    );
    let band = Perp::new(residual_market.perp, &w.provider)
        .makerDetails(residual_market.maker_id)
        .call()
        .await
        .unwrap();
    assert_eq!((band.tickLower.as_i32(), band.tickUpper.as_i32()), WIDE);

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
        // One atom less on either limit and the contract refuses the open.
        let raw = Perp::new(perp, &w.provider);
        let exact = OpenMakerParams {
            holder: w.me,
            margin: params.margin.atoms(),
            tickLower: params.range.lower().try_into().unwrap(),
            tickUpper: params.range.upper().try_into().unwrap(),
            liquidity: params.liquidity.units(),
            maxAmt0In: U256::from(expected("amount0_in")),
            maxAmt1In: U256::from(expected("amount1_in")),
            deadline: w.deadline().await,
        };
        raw.openMaker(exact.clone())
            .from(w.me)
            .call()
            .await
            .unwrap();
        for lowered in [
            OpenMakerParams {
                maxAmt0In: exact.maxAmt0In - U256::ONE,
                ..exact.clone()
            },
            OpenMakerParams {
                maxAmt1In: exact.maxAmt1In - U256::ONE,
                ..exact.clone()
            },
        ] {
            assert_eq!(
                revert_selector(&w, raw.openMaker(lowered)).await,
                Perp::MaxAmtExceeded::SELECTOR
            );
        }
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
        assert_eq!(sent.maxAmt0In, exact.maxAmt0In);
        assert_eq!(sent.maxAmt1In, exact.maxAmt1In);
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

    // A full removal that also withdraws margin.
    chain.refresh_gas().await.unwrap();
    let below = client
        .open_maker_v1(&params, Urgency::Normal)
        .await
        .unwrap();
    chain.refresh_gas().await.unwrap();
    let V1MakerAdjustment::Exit(MakerExit::Closed(withdrawn)) = client
        .adjust_maker_v1(
            &ExactAdjustMakerParams {
                pos_id: below.pos_id,
                margin_delta: UsdcDelta::new(-10_000_000),
                liquidity_delta: params.liquidity.negated().unwrap(),
                ..adjust
            },
            Urgency::Normal,
        )
        .await
        .unwrap()
    else {
        panic!("the below-price band closes");
    };
    let input = observe(&w, withdrawn.tx_hash, "maker-close-with-withdrawal").await;
    let sent = Perp::adjustMakerCall::abi_decode(&input).unwrap().params;
    assert_eq!(sent.marginDelta, -10_000_000);

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

/// The tagged Perp layout's `physicalCap`: the long X64 depth at slot 21,
/// the short one at 22 (`v1_positions_fork`).
const DEPTH_SLOT: u64 = 21;

/// Write both X64 close depths at the head, in a block of their own.
/// Anvil stores a block's state when it mines the next one, and
/// `anvil_setStorageAt` writes the live head, so an empty block comes
/// first and another after.
async fn set_depth(w: &V1Fork, perp: Address, depth: PerSide<U256>) {
    let mine = || async {
        let _: Value = w.provider.raw_request("evm_mine".into(), ()).await.unwrap();
    };
    mine().await;
    for (slot, word) in [(DEPTH_SLOT, depth.long), (DEPTH_SLOT + 1, depth.short)] {
        let _: Value = w
            .provider
            .raw_request(
                "anvil_setStorageAt".into(),
                (perp, B256::from(U256::from(slot)), B256::from(word)),
            )
            .await
            .unwrap();
    }
    mine().await;
}

async fn open_band(w: &mut V1Fork, perp: Address, lower: i32, upper: i32, liquidity: u128) -> U256 {
    let raw = Perp::new(perp, w.provider.clone());
    let id = raw.nextPosId().call().await.unwrap();
    let params = OpenMakerParams {
        holder: w.me,
        margin: v1_fork::MAKER_MARGIN,
        tickLower: lower.try_into().unwrap(),
        tickUpper: upper.try_into().unwrap(),
        liquidity,
        maxAmt0In: U256::MAX,
        maxAmt1In: U256::MAX,
        deadline: w.deadline().await,
    };
    w.send("open-band", raw.openMaker(params)).await;
    id
}

/// What a full removal of `pos_id` converts and the band contributes, at
/// the head: the open interest, the band's X64 contribution and the signed
/// residual (stored perp plus the principal the pool returns).
async fn full_removal(
    client: &PerpClient,
    pos_id: U256,
) -> (PerSide<PerpAtoms>, MakerBand, PerSide<U256>, i128) {
    let at = client.market().state_at_head().await.unwrap();
    let price = at.pool_sqrt_price().await.unwrap();
    let band = at.maker_band(pos_id).await.unwrap().unwrap();
    let perp = at.position(pos_id).await.unwrap().unwrap().perp.atoms();
    let interest = at.capacity().await.unwrap().open_interest;
    let (principal, _) = band_amounts(price, &band).unwrap();
    let residual = perp + i128::try_from(principal.atoms()).unwrap();
    (
        interest,
        band,
        band_capacity_x64(price, &band).unwrap(),
        residual,
    )
}

fn x64(atoms: PerpAtoms) -> U256 {
    U256::from(atoms.atoms()) << 64
}

/// Close-depth boundaries with open interest and two makers, through the
/// SDK and the chain: a partial removal, the converted residual, a removal
/// whose margin check fails first, a pending replacement, and the last
/// maker closing with its payout queued as a claim.
#[tokio::test]
#[ignore] // Requires Anvil and an Arbitrum Sepolia endpoint.
async fn v1_maker_close_depth_on_a_sepolia_fork() {
    let fork = SepoliaFork::start().await;
    let mut w = V1Fork::new(&fork);
    w.check_deploy().await;
    w.mint_usdc().await;
    let verifier = w.create_verifier().await;
    let beacon = w.create_beacon(verifier).await;
    w.bind(verifier, beacon).await;
    w.push_index("update-1", verifier, beacon, v1_fork::index_1())
        .await;
    let perp = w.create_perp(beacon).await;
    w.approve_usdc(perp).await;
    let maker_a = open_band(&mut w, perp, 38_700, 39_540, 335_000_000).await;
    let maker_b = open_band(&mut w, perp, 36_870, 40_950, 70_000_000).await;
    w.open_taker(perp).await;
    // No instant withdrawal capacity: every payout becomes a claim.
    let claims = w
        .create_perp_with(
            beacon,
            WithdrawalConfig {
                capacity: 0,
                refillPerSecond: 0,
                challengeWindow: U48::from(86_400u64),
            },
        )
        .await;
    w.approve_usdc(claims).await;
    let last = open_band(&mut w, claims, 38_400, 38_700, 200_000_000).await;

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
    let raw = Perp::new(perp, &w.provider);
    let nonce = || async { w.provider.get_transaction_count(w.me).await.unwrap() };
    let removal = |pos_id, margin_delta: i128, liquidity_delta: LDelta| ExactAdjustMakerParams {
        pos_id,
        margin_delta: UsdcDelta::new(margin_delta),
        liquidity_delta,
        amt0_limit: PerpAtoms::ZERO,
        amt1_limit: UsdcAtoms::ZERO,
    };
    let raw_removal = |params: &ExactAdjustMakerParams, deadline| AdjustMakerParams {
        posId: params.pos_id,
        marginDelta: params.margin_delta.atoms(),
        liquidityDelta: params.liquidity_delta.units(),
        amt0Limit: U256::ZERO,
        amt1Limit: U256::ZERO,
        deadline,
    };

    // ── A partial removal at the long boundary ──────────────────────
    let at = client.market().state_at_head().await.unwrap();
    let saved = at.physical_close_capacity_x64().await.unwrap();
    let interest = at.capacity().await.unwrap().open_interest;
    assert!(interest.long > PerpAtoms::ZERO && interest.short == PerpAtoms::ZERO);
    let price = at.pool_sqrt_price().await.unwrap();
    let band_a = at.maker_band(maker_a).await.unwrap().unwrap();
    assert!(at.maker_liquidity().await.unwrap() > U256::from(band_a.liquidity.units()));
    let part = band_capacity_x64(
        price,
        &MakerBand::new(band_a.range, LUnits::new(100_000_000)),
    )
    .unwrap();
    assert_ne!(part.short & U256::from(u64::MAX), U256::ZERO, "a fraction");
    let boundary = x64(interest.long) + part.short;
    assert!(boundary < saved.long);
    let partial = removal(maker_a, 0, LDelta::new(-100_000_000));
    set_depth(&w, perp, PerSide::new(boundary - U256::ONE, saved.short)).await;
    let before = nonce().await;
    let error = client
        .adjust_maker_v1(&partial, Urgency::Normal)
        .await
        .unwrap_err();
    assert!(
        matches!(error, PerpCityError::Validation(ValidationError::LongCloseCapacityExceeded {
            interest: refused, last_maker: false,
        }) if refused == interest.long.atoms()),
        "{error}"
    );
    assert_eq!(
        revert_selector(
            &w,
            raw.adjustMaker(raw_removal(&partial, w.deadline().await))
        )
        .await,
        Perp::LongCloseCapacityExceeded::SELECTOR
    );
    assert_eq!(nonce().await, before, "nothing sent");
    set_depth(&w, perp, PerSide::new(boundary, saved.short)).await;
    chain.refresh_gas().await.unwrap();
    let warnings = v1_fork::MakerWarnings::default();
    let V1MakerAdjustment::Adjusted(adjusted) = client
        .adjust_maker_v1(&partial, Urgency::Normal)
        .with_subscriber(warnings.clone())
        .await
        .unwrap()
    else {
        panic!("a partial removal stays a maker");
    };
    assert_eq!(
        warnings.count(),
        0,
        "the SDK's own check accepts the boundary; the contract did not overrule it"
    );
    observe(&w, adjusted.tx_hash, "maker-partial-removal-with-oi").await;
    let depth = raw.physicalCloseCapacity().call().await.unwrap();
    assert_eq!(
        depth.longClose,
        interest.long.atoms(),
        "the boundary left no long depth"
    );
    println!("partial removal boundary {boundary}: below refused, at it mined");

    // ── The residual check after a failing margin check ─────────────
    let (interest, band_b, whole_b, residual_b) = full_removal(&client, maker_b).await;
    assert!(residual_b < 0, "the taker's buy left maker B short");
    let long_fits = x64(interest.long) + whole_b.short;
    let short_boundary = x64(PerpAtoms::new(
        interest.short.atoms() + residual_b.unsigned_abs(),
    )) + whole_b.long;
    set_depth(
        &w,
        perp,
        PerSide::new(long_fits, short_boundary - U256::ONE),
    )
    .await;
    let margin = client
        .market()
        .get_position(maker_b)
        .await
        .unwrap()
        .margin
        .atoms();
    let starved = removal(
        maker_b,
        -i128::try_from(margin - 2_500_000).unwrap(),
        band_b.liquidity.negated().unwrap(),
    );
    let deadline = w.deadline().await;
    assert_eq!(
        revert_selector(&w, raw.adjustMaker(raw_removal(&starved, deadline))).await,
        Perp::MarginTooLow::SELECTOR
    );
    let unstarved = ExactAdjustMakerParams {
        margin_delta: UsdcDelta::ZERO,
        ..starved
    };
    assert_eq!(
        revert_selector(&w, raw.adjustMaker(raw_removal(&unstarved, deadline))).await,
        Perp::ShortCloseCapacityExceeded::SELECTOR,
        "without the withdrawal, the residual exceeds the short depth"
    );
    let before = nonce().await;
    let error = client
        .adjust_maker_v1(&starved, Urgency::Normal)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, PerpCityError::Transaction(inner) if inner.is_revert::<Perp::MarginTooLow>()),
        "the contract's first failure, not the residual's: {error}"
    );
    let error = client
        .adjust_maker_v1(&unstarved, Urgency::Normal)
        .await
        .unwrap_err();
    assert!(
        matches!(error, PerpCityError::Validation(ValidationError::ShortCloseCapacityExceeded {
            interest: refused, last_maker: false,
        }) if refused == interest.short.atoms() + residual_b.unsigned_abs()),
        "{error}"
    );
    assert_eq!(nonce().await, before, "nothing sent");
    println!("margin before residual: MarginTooLow returned");

    // ── The converted residual at the short boundary ────────────────
    let (interest, band_a, whole_a, residual_a) = full_removal(&client, maker_a).await;
    assert!(residual_a < 0, "the taker's buy left maker A short");
    let converted = interest.short.atoms() + residual_a.unsigned_abs();
    let long_fits = x64(interest.long) + whole_a.short;
    let short_boundary = x64(PerpAtoms::new(converted)) + whole_a.long;
    set_depth(
        &w,
        perp,
        PerSide::new(long_fits, short_boundary - U256::ONE),
    )
    .await;
    let before = nonce().await;
    let error = client
        .close_maker_v1(maker_a, Urgency::Normal)
        .await
        .unwrap_err();
    assert!(
        matches!(error, PerpCityError::Validation(ValidationError::ShortCloseCapacityExceeded {
            interest: refused, last_maker: false,
        }) if refused == converted),
        "{error}"
    );
    let close = removal(maker_a, 0, band_a.liquidity.negated().unwrap());
    assert_eq!(
        revert_selector(&w, raw.adjustMaker(raw_removal(&close, w.deadline().await))).await,
        Perp::ShortCloseCapacityExceeded::SELECTOR
    );
    assert_eq!(nonce().await, before, "nothing sent");
    set_depth(&w, perp, PerSide::new(long_fits, short_boundary)).await;
    chain.refresh_gas().await.unwrap();
    let warnings = v1_fork::MakerWarnings::default();
    let MakerExit::Converted(conversion) = client
        .close_maker_v1(maker_a, Urgency::Normal)
        .with_subscriber(warnings.clone())
        .await
        .unwrap()
    else {
        panic!("maker A converts its residual");
    };
    assert_eq!(
        warnings.count(),
        0,
        "the SDK's own check accepts the boundary; the contract did not overrule it"
    );
    observe(&w, conversion.tx_hash, "maker-convert-with-oi").await;
    let oi = raw.openInterest().call().await.unwrap();
    assert_eq!(oi.short, converted);
    let depth = raw.physicalCloseCapacity().call().await.unwrap();
    assert_eq!(
        depth.shortClose, converted,
        "the boundary left no short depth"
    );
    println!("residual boundary {short_boundary}: below refused, at it converted");

    // ── A pending maker open, replaced with automine off ────────────
    set_depth(&w, perp, saved).await;
    let _: Value = w
        .provider
        .raw_request("evm_setAutomine".into(), (false,))
        .await
        .unwrap();
    chain.refresh_gas().await.unwrap();
    let error = client
        .open_maker_v1(&open_params(38_700, 39_540, 50_000_000), Urgency::Normal)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            PerpCityError::Transaction(TransactionError::ReceiptTimeout { .. })
        ),
        "{error}"
    );
    let stuck = error.tx_hash().unwrap();
    let stuck_tx = w
        .provider
        .get_transaction_by_hash(stuck)
        .await
        .unwrap()
        .unwrap();
    let stuck_call = Perp::openMakerCall::abi_decode(stuck_tx.input())
        .unwrap()
        .params;
    sleep(Duration::from_secs(2)).await;
    // The receipt timeout outlived the gas cache.
    chain.refresh_gas().await.unwrap();
    let replaced_at = wall_secs();
    let (replaced, _) = tokio::join!(client.replace_v1_maker(stuck, Urgency::High), async {
        sleep(Duration::from_secs(3)).await;
        let _: Value = w.provider.raw_request("evm_mine".into(), ()).await.unwrap();
    });
    let V1MakerReplacement::Opened(replaced) = replaced.unwrap() else {
        panic!("an open replaced as an open");
    };
    let _: Value = w
        .provider
        .raw_request("evm_setAutomine".into(), (true,))
        .await
        .unwrap();
    assert_ne!(replaced.tx_hash, stuck, "the replacement mined");
    let replacement_tx = w
        .provider
        .get_transaction_by_hash(replaced.tx_hash)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replacement_tx.nonce(), stuck_tx.nonce(), "same nonce");
    let renewed = Perp::openMakerCall::abi_decode(replacement_tx.input())
        .unwrap()
        .params;
    assert!(renewed.deadline > stuck_call.deadline);
    assert!((replaced_at + 600..=wall_secs() + 600).contains(&renewed.deadline));
    let unchanged = OpenMakerParams {
        deadline: stuck_call.deadline,
        ..renewed.clone()
    };
    assert_eq!(
        Perp::openMakerCall { params: unchanged }.abi_encode(),
        Perp::openMakerCall { params: stuck_call }.abi_encode(),
        "only the deadline changed"
    );
    assert!(
        w.provider
            .get_transaction_receipt(stuck)
            .await
            .unwrap()
            .is_none(),
        "the stuck open did not mine"
    );
    assert_eq!(replaced.event.liquidity, 50_000_000);
    observe(&w, replaced.tx_hash, "maker-open-replacement").await;
    println!("stuck maker open {stuck} replaced by {}", replaced.tx_hash);

    // ── The last maker, no residual: closed, its payout a claim ─────
    let closer = PerpClient::new(chain.market(claims), w.signer.clone());
    closer.sync_nonce().await.unwrap();
    let (interest, band, _, residual) = full_removal(&closer, last).await;
    assert_eq!(interest, PerSide::new(PerpAtoms::ZERO, PerpAtoms::ZERO));
    assert_eq!(residual, 0, "below the price the band holds no perp");
    let at = closer.market().state_at_head().await.unwrap();
    assert_eq!(
        at.maker_liquidity().await.unwrap(),
        U256::from(band.liquidity.units())
    );
    let controller = Perp::new(claims, &w.provider)
        .withdrawalController()
        .call()
        .await
        .unwrap();
    let escrowed = w.usdc_balance(controller).await;
    let held = w.usdc_balance(w.me).await;
    chain.refresh_gas().await.unwrap();
    let MakerExit::Closed(closed) = closer.close_maker_v1(last, Urgency::Normal).await.unwrap()
    else {
        panic!("the last maker with no residual closes");
    };
    observe(&w, closed.tx_hash, "maker-close-last-claim").await;
    assert!(
        w.usdc_balance(controller).await > escrowed,
        "the payout is a claim"
    );
    assert_eq!(w.usdc_balance(w.me).await, held, "nothing paid out at once");
    assert_eq!(
        Perp::new(claims, &w.provider)
            .positionOwner(last)
            .call()
            .await
            .unwrap(),
        Address::ZERO
    );
    println!("\n=== v1 maker close-depth fork test passed ===");
}
