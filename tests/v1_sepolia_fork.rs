//! Integration test: the `v1.0.0` bindings against deployed bytecode. A
//! market is built on an Anvil fork of Arbitrum Sepolia by the W11 smoke
//! steps (`v1_fork`), and every log its steps emit, every view it answers
//! and three of its reverts are decoded with `contracts::v1`. Each decode
//! is also re-encoded and compared with the bytes on the wire, so a field
//! the binding lacks or adds fails here and not as a silent prefix decode.
//!
//! Requires `anvil` (from Foundry) and an Arbitrum Sepolia endpoint in
//! `SEPOLIA_FORK_URL` (default: the public RPC).
//!
//! ```bash
//! cargo test --test v1_sepolia_fork -- --ignored --nocapture
//! ```

mod v1_fork;

use std::collections::BTreeMap;

use alloy::primitives::{Address, Bytes, I256, IntoLogData, U256, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::{SolCall, SolEventInterface, SolInterface, SolValue};

use perpcity_sdk::contracts::v1::{
    AdjustMakerParams, IBeacon, IFees, IFunding, IMarginRatios, IPriceImpact, IPricing,
    OpenTakerParams, Perp, PerpFactory,
};

use v1_fork::{
    FEES, FUNDING, HOOK, MARGIN_RATIOS, PRICE_IMPACT, PRICING, SepoliaFork, V1Fork, V1Market,
};

/// The `v1.0.0` events each W11 step must emit from the market (W11
/// table; Uniswap, ERC-20, proxy and controller logs are not listed).
const STEP_EVENTS: &[(&str, &[&str])] = &[
    ("create-perp", &["Upgraded", "Initialized", "SetGovernor"]),
    (
        "open-maker",
        &[
            "MakerOpened",
            "PositionOwnerUpdated",
            "MarginTransferred",
            "TickInitialized",
            "RatesAndEmasRefreshed",
            "CapacityUpdated",
            "PhysicalCapacityUpdated",
            "CumulativesAccrued",
        ],
    ),
    (
        "open-taker",
        &[
            "TakerOpened",
            "PositionOwnerUpdated",
            "TicksCrossed",
            "SwapFeesRemovedFromMargin",
            "OpenInterestUpdated",
            "RatesAndEmasRefreshed",
        ],
    ),
    (
        "touch",
        &[
            "RatesAndEmasRefreshed",
            "UtilizationAccrued",
            "CumulativesAccrued",
            "CapacityUpdated",
        ],
    ),
    (
        "close-taker",
        &["TakerClosed", "PositionOwnerUpdated", "MarginTransferred"],
    ),
    (
        "move-maker",
        &["MakerMoved", "TickDeleted", "TickInitialized"],
    ),
    ("reduce-maker", &["MakerAdjusted"]),
];

#[tokio::test]
#[ignore] // Requires `anvil` and an Arbitrum Sepolia endpoint.
async fn v1_bindings_decode_a_w11_market() {
    let fork = SepoliaFork::start(48_700).await;
    let mut w = V1Fork::new(&fork);
    let market = w.build_w11_market().await;
    println!(
        "v1 market {} (beacon {}, maker {}, taker {})",
        market.perp, market.beacon, market.maker_id, market.taker_id
    );

    let per_step = decode_logs(&w, &market);
    for (step, expected) in STEP_EVENTS {
        let seen = &per_step[step];
        for name in *expected {
            assert!(seen.contains(name), "{step}: no {name} in {seen:?}");
        }
    }
    for step in &w.steps {
        println!("{}: {:?}", step.label, per_step[step.label]);
    }
    let total: usize = per_step.values().map(Vec::len).sum();
    println!("{total} logs decoded across {} steps", w.steps.len());

    check_views(&w, &market).await;
    check_reverts(&w, &market).await;
}

/// Decode every log of every step with the `v1` binding of its emitter,
/// re-encode it, and compare. Returns the event names per step.
fn decode_logs(w: &V1Fork, market: &V1Market) -> BTreeMap<&'static str, Vec<&'static str>> {
    let mut out: BTreeMap<&'static str, Vec<&'static str>> = BTreeMap::new();
    for step in &w.steps {
        let names = out.entry(step.label).or_default();
        for log in step.receipt.inner.logs() {
            let data = log.data();
            let topic0 = data.topics()[0];
            let name = if log.address() == market.perp {
                let event = Perp::PerpEvents::decode_raw_log(data.topics(), &data.data)
                    .unwrap_or_else(|e| panic!("{}: perp log {topic0}: {e}", step.label));
                assert_eq!(
                    &event.to_log_data(),
                    data,
                    "{}: perp log {topic0} re-encodes differently",
                    step.label
                );
                Perp::PerpEvents::name_by_selector(topic0.0).unwrap()
            } else if log.address() == market.beacon {
                let event = IBeacon::IBeaconEvents::decode_raw_log(data.topics(), &data.data)
                    .unwrap_or_else(|e| panic!("{}: beacon log: {e}", step.label));
                assert_eq!(&event.to_log_data(), data);
                "IndexUpdated"
            } else if log.address() == v1_fork::FACTORY {
                let event =
                    PerpFactory::PerpFactoryEvents::decode_raw_log(data.topics(), &data.data)
                        .unwrap_or_else(|e| panic!("{}: factory log: {e}", step.label));
                assert_eq!(&event.to_log_data(), data);
                if let PerpFactory::PerpFactoryEvents::PerpCreated(created) = &event {
                    assert_eq!(created.perp, market.perp);
                    assert_eq!(created.emaWindow.to::<u32>(), v1_fork::EMA_WINDOW);
                    assert_eq!(created.latency, v1_fork::LATENCY);
                    assert_eq!(created.modules.beacon, market.beacon);
                    assert_eq!(created.owner, w.me);
                }
                PerpFactory::PerpFactoryEvents::name_by_selector(topic0.0).unwrap()
            } else {
                continue;
            };
            names.push(name);
        }
    }
    for name in ["Upgraded", "Initialized", "PerpCreated"] {
        assert!(out["create-perp"].contains(&name), "create-perp: no {name}");
    }
    assert_eq!(out["update-1"], ["IndexUpdated"]);
    assert_eq!(out["update-2"], ["IndexUpdated"]);
    out
}

/// `eth_call` `call` on `to`, decode the answer strictly, and check it
/// re-encodes to the same bytes (the binding has exactly the deployed
/// return shape).
async fn view<C: SolCall>(w: &V1Fork, to: Address, call: C) -> C::Return {
    let tx = TransactionRequest::default()
        .to(to)
        .input(Bytes::from(call.abi_encode()).into());
    let ret = w
        .provider
        .call(tx)
        .await
        .unwrap_or_else(|e| panic!("{}: {e}", C::SIGNATURE));
    let decoded =
        C::abi_decode_returns_validate(&ret).unwrap_or_else(|e| panic!("{}: {e}", C::SIGNATURE));
    assert_eq!(
        C::abi_encode_returns(&decoded),
        ret.to_vec(),
        "{}: the deployed return has another shape",
        C::SIGNATURE
    );
    decoded
}

async fn check_views(w: &V1Fork, m: &V1Market) {
    let p = m.perp;

    // The pool key, its id and the guard hook.
    let key = view(w, p, Perp::poolKeyCall {}).await;
    assert_eq!(key.hooks, HOOK);
    assert_eq!(view(w, p, Perp::hooksCall {}).await, HOOK);
    assert_eq!(view(w, p, Perp::currency0Call {}).await, key.currency0);
    assert_eq!(view(w, p, Perp::currency1Call {}).await, key.currency1);
    assert_eq!(
        view(w, p, Perp::poolIdCall {}).await,
        keccak256(key.abi_encode()),
        "poolId() is keccak256(abi.encode(poolKey()))"
    );

    // Configuration.
    assert_eq!(
        view(w, p, Perp::emaWindowCall {}).await.to::<u32>(),
        v1_fork::EMA_WINDOW
    );
    assert_eq!(view(w, p, Perp::latencyCall {}).await, v1_fork::LATENCY);
    assert_eq!(view(w, p, Perp::ownerCall {}).await, w.me);
    assert_eq!(view(w, p, Perp::governorCall {}).await, w.me);
    let modules = view(w, p, Perp::modulesCall {}).await;
    assert_eq!(modules.beacon, m.beacon);
    assert_eq!(modules.fees, FEES);
    assert_ne!(
        view(w, p, Perp::withdrawalControllerCall {}).await,
        Address::ZERO
    );
    assert_ne!(
        view(w, p, Perp::protocolFeeManagerCall {}).await,
        Address::ZERO
    );
    assert_eq!(view(w, p, Perp::tvlCapCall {}).await, U256::MAX);
    view(w, p, Perp::protocolFeeCall {}).await;
    view(w, p, Perp::permissionlessDonationsEnabledCall {}).await;
    view(
        w,
        p,
        Perp::abdicatedCall {
            selector: Perp::openTakerCall::SELECTOR.into(),
        },
    )
    .await;

    // Market state.
    let rates = view(w, p, Perp::ratesCall {}).await;
    assert!(rates.lastMark > U256::ZERO);
    view(w, p, Perp::cumulativesCall {}).await;
    view(w, p, Perp::capacityCall {}).await;
    view(w, p, Perp::openInterestCall {}).await;
    view(w, p, Perp::feeFundCall {}).await;
    view(w, p, Perp::solvencyStateCall {}).await;
    view(w, p, Perp::fundingStateCall {}).await;
    view(w, p, Perp::utilizationBandCall {}).await;
    view(w, p, Perp::accountedTvlCall {}).await;
    view(w, p, Perp::physicalCloseCapacityCall {}).await;
    view(w, p, Perp::poolStateCall {}).await;
    assert_eq!(
        view(w, p, Perp::nextPosIdCall {}).await,
        m.taker_id + U256::from(1)
    );
    let index = view(w, p, Perp::readBeaconIndexCall { beacon: m.beacon }).await;
    assert_eq!(U256::from(index), v1_fork::index_2());

    // Positions: the open maker, and the closed taker's cleared row.
    assert_eq!(
        view(w, p, Perp::positionOwnerCall { posId: m.maker_id }).await,
        w.me
    );
    assert_eq!(
        view(w, p, Perp::positionOwnerCall { posId: m.taker_id }).await,
        Address::ZERO
    );
    let maker = view(w, p, Perp::makerDetailsCall { posId: m.maker_id }).await;
    assert_eq!(
        (maker.tickLower.as_i32(), maker.tickUpper.as_i32()),
        v1_fork::MOVE_TICKS
    );
    assert_eq!(
        maker.liquidity,
        v1_fork::MOVE_LIQUIDITY - v1_fork::REDUCE_LIQUIDITY
    );
    let position = view(w, p, Perp::positionsCall { posId: m.maker_id }).await;
    assert!(position.margin > 0);
    view(w, p, Perp::takerDetailsCall { posId: m.taker_id }).await;
    let preview = view(w, p, Perp::previewPositionCall { posId: m.maker_id }).await;
    assert!(preview.isMaker);
    assert!(preview.markPrice > U256::ZERO);

    // The modules and the beacon.
    view(w, FEES, IFees::maxSwapFeeCall {}).await;
    view(w, FEES, IFees::liqFeeCall {}).await;
    view(w, FEES, IFees::cFeeCall {}).await;
    view(w, FEES, IFees::insFeeCall {}).await;
    view(w, FEES, IFees::lpFeeConfigCall {}).await;
    view(w, FEES, IFees::targetUtilizationCall {}).await;
    view(w, FEES, IFees::slopeBeforeTargetCall {}).await;
    view(w, FEES, IFees::slopeAfterTargetCall {}).await;
    view(
        w,
        FEES,
        IFees::utilFeesCall {
            longUtilization: U256::ZERO,
            shortUtilization: U256::ZERO,
        },
    )
    .await;
    view(w, FUNDING, IFunding::fundingRateMultiplierCall {}).await;
    view(w, FUNDING, IFunding::maxFundingPerDayPercentCall {}).await;
    let ratios = view(w, MARGIN_RATIOS, IMarginRatios::makerMarginRatiosCall {}).await;
    assert_eq!(
        ratios.init.to::<u32>(),
        1_000_000,
        "the Sepolia maker initial ratio (W11 fact 2)"
    );
    view(w, MARGIN_RATIOS, IMarginRatios::takerMarginRatiosCall {}).await;
    view(w, MARGIN_RATIOS, IMarginRatios::pendingMakerRatiosCall {}).await;
    view(w, MARGIN_RATIOS, IMarginRatios::UPDATE_DELAYCall {}).await;
    view(w, PRICE_IMPACT, IPriceImpact::priceImpactBoundCall {}).await;
    view(w, PRICING, IPricing::maxPremiumRatioCall {}).await;
    let beacon = view(w, m.beacon, IBeacon::indexCall {}).await;
    assert_eq!(beacon.index, v1_fork::index_2());
    assert!(beacon.updatedAt > U256::ZERO);
}

/// The typed reverts of three refusals decode by the `v1` error binding.
async fn check_reverts(w: &V1Fork, m: &V1Market) {
    // A closed id has no preview.
    let call = Perp::previewPositionCall { posId: m.taker_id };
    assert_eq!(
        revert(w, m.perp, call.abi_encode()).await,
        "PositionDoesNotExist"
    );

    // A trade past its deadline.
    let call = Perp::openTakerCall {
        params: OpenTakerParams {
            holder: w.me,
            margin: v1_fork::TAKER_MARGIN,
            perpDelta: I256::try_from(v1_fork::TAKER_SIZE).unwrap(),
            amt1Limit: U256::MAX,
            maxTotalFee: U256::from(v1_fork::MAX_TOTAL_FEE),
            deadline: 1,
        },
    };
    assert_eq!(
        revert(w, m.perp, call.abi_encode()).await,
        "DeadlineExpired"
    );

    // The only maker cannot close in full with a residual short (W11 fact 1).
    let maker = view(w, m.perp, Perp::makerDetailsCall { posId: m.maker_id }).await;
    let call = Perp::adjustMakerCall {
        params: AdjustMakerParams {
            posId: m.maker_id,
            marginDelta: 0,
            liquidityDelta: -(maker.liquidity as i128),
            amt0Limit: U256::ZERO,
            amt1Limit: U256::ZERO,
            deadline: w.deadline().await,
        },
    };
    assert_eq!(
        revert(w, m.perp, call.abi_encode()).await,
        "ShortCloseCapacityExceeded"
    );
}

/// The name of the `v1` error an `eth_call` from the dev wallet reverts with.
async fn revert(w: &V1Fork, to: Address, data: Vec<u8>) -> &'static str {
    let tx = TransactionRequest::default()
        .from(w.me)
        .to(to)
        .input(Bytes::from(data).into());
    let err = w.provider.call(tx).await.expect_err("the call must revert");
    let payload = err
        .as_error_resp()
        .and_then(|e| e.as_revert_data())
        .unwrap_or_else(|| panic!("no revert data: {err}"));
    let decoded = Perp::PerpErrors::abi_decode(&payload)
        .unwrap_or_else(|e| panic!("revert {payload} is not a v1 error: {e}"));
    Perp::PerpErrors::name_by_selector(decoded.selector()).unwrap()
}
