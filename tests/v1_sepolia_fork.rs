//! Integration test: the `v1.0.0` bindings against deployed bytecode. A
//! market is built on an Anvil fork of Arbitrum Sepolia by the W11 smoke
//! steps (`v1_fork`), and every log its steps emit from the market, its
//! factory, its withdrawal controller and its beacon, every view of every
//! binding, and three of its reverts are decoded with `contracts::v1`.
//! Each decode is also re-encoded and compared with the bytes on the
//! wire, so a field the binding lacks or adds fails here and not as a
//! silent prefix decode.
//!
//! Requires `anvil` (from Foundry) and an Arbitrum Sepolia endpoint in
//! `SEPOLIA_FORK_URL` (default: the public RPC).
//!
//! ```bash
//! cargo test --test v1_sepolia_fork -- --ignored --nocapture
//! ```

mod v1_fork;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use alloy::json_abi::{JsonAbi, StateMutability};
use alloy::primitives::{Address, B256, Bytes, I256, IntoLogData, U256, b256, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::{SolCall, SolEventInterface, SolInterface, SolValue};

use perpcity_sdk::contracts::v1::{
    AdjustMakerParams, ClaimStatus, IBeacon, IFees, IFunding, IMarginRatios, IPriceImpact,
    IPricing, OpenTakerParams, Perp, PerpFactory, PricePair, WithdrawalController,
};

use v1_fork::{
    BLOCK_INTERVAL, FACTORY, FEES, FUNDING, HOOK, MARGIN_RATIOS, PERP_IMPLEMENTATION, PRICE_IMPACT,
    PRICING, SepoliaFork, V1Fork, V1Market,
};

/// The ERC-1967 implementation slot, `proxiableUUID()` of a UUPS
/// implementation.
const ERC1967_IMPLEMENTATION_SLOT: B256 =
    b256!("360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc");

/// The `v1.0.0` events each W11 step must emit from the market and its
/// controller (W11 table; Uniswap and ERC-20 logs are not listed).
const STEP_EVENTS: &[(&str, &[&str])] = &[
    (
        "create-perp",
        &[
            "Upgraded",
            "Initialized",
            "SetGovernor",
            "PerpCreated",
            "ControllerInitialized",
        ],
    ),
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
        &[
            "TakerClosed",
            "PositionOwnerUpdated",
            "MarginTransferred",
            "InstantPayout",
        ],
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
    let fork = SepoliaFork::start().await;
    let mut w = V1Fork::new(&fork);
    let market = w.build_w11_market().await;
    println!(
        "v1 market {} (controller {}, beacon {}, maker {}, taker {})",
        market.perp, market.controller, market.beacon, market.maker_id, market.taker_id
    );
    check_clock(&w);

    let per_step = decode_logs(&w, &market);
    for (step, expected) in STEP_EVENTS {
        let seen = &per_step[step];
        for name in *expected {
            assert!(seen.contains(name), "{step}: no {name} in {seen:?}");
        }
    }
    for step in &w.steps {
        println!(
            "{}: t={} gas used {} estimate {:?}: {:?}",
            step.label, step.timestamp, step.receipt.gas_used, step.estimate, per_step[step.label]
        );
    }
    let total: usize = per_step.values().map(Vec::len).sum();
    println!("{total} logs decoded across {} steps", w.steps.len());

    let views = Views::new(&w, &market);
    check_views(&views).await;
    check_reverts(&views).await;
    views.check_all_called();
}

/// Each step mines later than the one before it, and the touch a minute
/// after the second index: the market accrues on every step.
fn check_clock(w: &V1Fork) {
    for pair in w.steps.windows(2) {
        assert!(
            pair[1].timestamp >= pair[0].timestamp + BLOCK_INTERVAL,
            "{} mined at {}, {} at {}",
            pair[0].label,
            pair[0].timestamp,
            pair[1].label,
            pair[1].timestamp
        );
    }
    let at = |label: &str| w.steps.iter().find(|s| s.label == label).unwrap().timestamp;
    assert_eq!(
        at("touch") - at("update-2"),
        60,
        "the warp did not move the clock"
    );
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
            } else if log.address() == market.controller {
                let event = WithdrawalController::WithdrawalControllerEvents::decode_raw_log(
                    data.topics(),
                    &data.data,
                )
                .unwrap_or_else(|e| panic!("{}: controller log: {e}", step.label));
                assert_eq!(&event.to_log_data(), data);
                if let WithdrawalController::WithdrawalControllerEvents::ControllerInitialized(
                    init,
                ) = &event
                {
                    assert_eq!(init.perp, market.perp);
                    assert_eq!(init.capacity, u128::MAX);
                }
                WithdrawalController::WithdrawalControllerEvents::name_by_selector(topic0.0)
                    .unwrap()
            } else if log.address() == FACTORY {
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
    assert_eq!(out["update-1"], ["IndexUpdated"]);
    assert_eq!(out["update-2"], ["IndexUpdated"]);
    out
}

/// The bindings with views, by the name the call log keys them by.
fn view_bindings() -> [(&'static str, JsonAbi); 9] {
    [
        ("Perp", Perp::abi::contract()),
        ("PerpFactory", PerpFactory::abi::contract()),
        (
            "WithdrawalController",
            WithdrawalController::abi::contract(),
        ),
        ("IFees", IFees::abi::contract()),
        ("IFunding", IFunding::abi::contract()),
        ("IMarginRatios", IMarginRatios::abi::contract()),
        ("IPriceImpact", IPriceImpact::abi::contract()),
        ("IPricing", IPricing::abi::contract()),
        ("IBeacon", IBeacon::abi::contract()),
    ]
}

/// Strict `eth_call`s of views, with a log of each binding and signature
/// called, so the test shows that it called every view of every binding.
struct Views<'a> {
    w: &'a V1Fork,
    m: &'a V1Market,
    called: RefCell<BTreeSet<String>>,
}

impl<'a> Views<'a> {
    fn new(w: &'a V1Fork, m: &'a V1Market) -> Self {
        Self {
            w,
            m,
            called: RefCell::new(BTreeSet::new()),
        }
    }

    /// The binding whose ABI `to` answers.
    fn binding(&self, to: Address) -> &'static str {
        match to {
            a if a == self.m.perp || a == PERP_IMPLEMENTATION => "Perp",
            a if a == FACTORY => "PerpFactory",
            a if a == self.m.controller => "WithdrawalController",
            a if a == FEES => "IFees",
            a if a == FUNDING => "IFunding",
            a if a == MARGIN_RATIOS => "IMarginRatios",
            a if a == PRICE_IMPACT => "IPriceImpact",
            a if a == PRICING => "IPricing",
            a if a == self.m.beacon => "IBeacon",
            _ => panic!("no binding for {to}"),
        }
    }

    /// `eth_call` `call` on `to`, decode the answer strictly, and check it
    /// re-encodes to the same bytes (the binding has exactly the deployed
    /// return shape).
    async fn view<C: SolCall>(&self, to: Address, call: C) -> C::Return {
        self.called
            .borrow_mut()
            .insert(format!("{}.{}", self.binding(to), C::SIGNATURE));
        let tx = TransactionRequest::default()
            .to(to)
            .input(Bytes::from(call.abi_encode()).into());
        let ret = self
            .w
            .provider
            .call(tx)
            .await
            .unwrap_or_else(|e| panic!("{}: {e}", C::SIGNATURE));
        let decoded = C::abi_decode_returns_validate(&ret)
            .unwrap_or_else(|e| panic!("{}: {e}", C::SIGNATURE));
        assert_eq!(
            C::abi_encode_returns(&decoded),
            ret.to_vec(),
            "{}: the deployed return has another shape",
            C::SIGNATURE
        );
        decoded
    }

    fn check_all_called(&self) {
        let every: BTreeSet<String> = view_bindings()
            .iter()
            .flat_map(|(name, abi)| {
                abi.functions()
                    .filter(|f| {
                        matches!(
                            f.state_mutability,
                            StateMutability::View | StateMutability::Pure
                        )
                    })
                    .map(move |f| format!("{name}.{}", f.signature()))
            })
            .collect();
        let called = self.called.borrow();
        let missed: Vec<_> = every.difference(&called).collect();
        assert!(missed.is_empty(), "views never called: {missed:#?}");
        println!("{} views called, every view of every binding", called.len());
    }
}

async fn check_views(v: &Views<'_>) {
    let (w, m, p) = (v.w, v.m, v.m.perp);

    // The pool key, its id and the guard hook.
    let key = v.view(p, Perp::poolKeyCall {}).await;
    assert_eq!(key.hooks, HOOK);
    assert_eq!(v.view(p, Perp::hooksCall {}).await, HOOK);
    assert_eq!(v.view(p, Perp::currency0Call {}).await, key.currency0);
    assert_eq!(v.view(p, Perp::currency1Call {}).await, key.currency1);
    assert_eq!(
        v.view(p, Perp::poolIdCall {}).await,
        keccak256(key.abi_encode()),
        "poolId() is keccak256(abi.encode(poolKey()))"
    );

    // Configuration, ownership and the proxy.
    assert_eq!(
        v.view(p, Perp::emaWindowCall {}).await.to::<u32>(),
        v1_fork::EMA_WINDOW
    );
    assert_eq!(v.view(p, Perp::latencyCall {}).await, v1_fork::LATENCY);
    assert_eq!(v.view(p, Perp::ownerCall {}).await, w.me);
    assert_eq!(v.view(p, Perp::governorCall {}).await, w.me);
    let modules = v.view(p, Perp::modulesCall {}).await;
    assert_eq!(modules.beacon, m.beacon);
    assert_eq!(
        [
            modules.fees,
            modules.funding,
            modules.marginRatios,
            modules.priceImpact,
            modules.pricing
        ],
        [FEES, FUNDING, MARGIN_RATIOS, PRICE_IMPACT, PRICING]
    );
    assert_eq!(
        v.view(p, Perp::withdrawalControllerCall {}).await,
        m.controller
    );
    let fee_manager = v.view(p, Perp::protocolFeeManagerCall {}).await;
    assert_ne!(fee_manager, Address::ZERO);
    assert_eq!(v.view(p, Perp::tvlCapCall {}).await, U256::MAX);
    v.view(p, Perp::protocolFeeCall {}).await;
    v.view(p, Perp::permissionlessDonationsEnabledCall {}).await;
    let abdicated = Perp::abdicatedCall {
        selector: Perp::openTakerCall::SELECTOR.into(),
    };
    assert!(!v.view(p, abdicated).await);
    let handover = Perp::ownershipHandoverExpiresAtCall { pendingOwner: w.me };
    assert_eq!(v.view(p, handover).await, U256::ZERO);
    assert_eq!(
        v.view(p, Perp::UPGRADE_INTERFACE_VERSIONCall {}).await,
        "5.0.0"
    );
    // UUPS answers `proxiableUUID` on the implementation only.
    assert_eq!(
        v.view(PERP_IMPLEMENTATION, Perp::proxiableUUIDCall {})
            .await,
        ERC1967_IMPLEMENTATION_SLOT
    );

    // Market state.
    let rates = v.view(p, Perp::ratesCall {}).await;
    assert!(rates.lastMark > U256::ZERO);
    v.view(p, Perp::cumulativesCall {}).await;
    v.view(p, Perp::capacityCall {}).await;
    v.view(p, Perp::openInterestCall {}).await;
    v.view(p, Perp::feeFundCall {}).await;
    v.view(p, Perp::solvencyStateCall {}).await;
    v.view(p, Perp::fundingStateCall {}).await;
    v.view(p, Perp::utilizationBandCall {}).await;
    v.view(p, Perp::accountedTvlCall {}).await;
    v.view(p, Perp::physicalCloseCapacityCall {}).await;
    let pool = v.view(p, Perp::poolStateCall {}).await;
    assert!(pool.liquidity > 0);
    assert_eq!(
        v.view(p, Perp::nextPosIdCall {}).await,
        m.taker_id + U256::from(1)
    );
    let index = v
        .view(p, Perp::readBeaconIndexCall { beacon: m.beacon })
        .await;
    assert_eq!(U256::from(index), v1_fork::index_2());

    // Positions: the open maker, and the closed taker's cleared row.
    assert_eq!(
        v.view(p, Perp::positionOwnerCall { posId: m.maker_id })
            .await,
        w.me
    );
    assert_eq!(
        v.view(p, Perp::positionOwnerCall { posId: m.taker_id })
            .await,
        Address::ZERO
    );
    let maker = v
        .view(p, Perp::makerDetailsCall { posId: m.maker_id })
        .await;
    assert_eq!(
        (maker.tickLower.as_i32(), maker.tickUpper.as_i32()),
        v1_fork::MOVE_TICKS
    );
    assert_eq!(
        maker.liquidity,
        v1_fork::MOVE_LIQUIDITY - v1_fork::REDUCE_LIQUIDITY
    );
    let position = v.view(p, Perp::positionsCall { posId: m.maker_id }).await;
    assert!(position.margin > 0);
    v.view(p, Perp::takerDetailsCall { posId: m.taker_id })
        .await;
    let preview = v
        .view(p, Perp::previewPositionCall { posId: m.maker_id })
        .await;
    assert!(preview.isMaker);
    assert!(preview.markPrice > U256::ZERO);

    // The factory.
    let f = FACTORY;
    assert_eq!(
        v.view(f, PerpFactory::perpImplementationCall {}).await,
        PERP_IMPLEMENTATION
    );
    assert_eq!(v.view(f, PerpFactory::PERP_GUARD_HOOKCall {}).await, HOOK);
    assert_eq!(
        v.view(f, PerpFactory::PROTOCOL_FEE_MANAGERCall {}).await,
        fee_manager
    );
    assert_ne!(
        v.view(f, PerpFactory::ACCOUNTING_TOKEN_IMPLCall {}).await,
        Address::ZERO
    );
    let controller_impl = v
        .view(f, PerpFactory::WITHDRAWAL_CONTROLLER_IMPLCall {})
        .await;
    assert_ne!(controller_impl, Address::ZERO);
    assert_ne!(
        controller_impl, m.controller,
        "the market's controller is a clone"
    );

    // The withdrawal controller: an unlimited bucket, so the taker's
    // payout was instant and no claim exists.
    let c = m.controller;
    assert_eq!(v.view(c, WithdrawalController::perpCall {}).await, p);
    assert_eq!(
        v.view(c, WithdrawalController::challengeWindowCall {})
            .await
            .to::<u64>(),
        86_400
    );
    let bucket = v.view(c, WithdrawalController::rateLimitCall {}).await;
    assert_eq!(
        (bucket.capacity, bucket.refillPerSecond),
        (u128::MAX, u128::MAX)
    );
    assert!(!v.view(c, WithdrawalController::releasesPausedCall {}).await);
    // Claim ids start at one; the first is still unused.
    assert_eq!(
        v.view(c, WithdrawalController::nextClaimIdCall {}).await,
        U256::from(1)
    );
    assert_eq!(
        v.view(c, WithdrawalController::totalReservedCall {}).await,
        U256::ZERO
    );
    let claim = v
        .view(
            c,
            WithdrawalController::claimsCall {
                claimId: U256::from(1),
            },
        )
        .await;
    assert!(matches!(claim.status, ClaimStatus::None));

    // The modules, with the market's own prices as the inputs.
    let spots = PricePair {
        ammPrice: pool.ammPrice.to::<u128>(),
        index,
    };
    let emas = spots.clone();
    let c_fee = v.view(FEES, IFees::cFeeCall {}).await;
    let ins_fee = v.view(FEES, IFees::insFeeCall {}).await;
    let fees = v
        .view(
            FEES,
            IFees::feesCall {
                liquidity: pool.liquidity,
                spots: spots.clone(),
                emas: emas.clone(),
            },
        )
        .await;
    assert_eq!((fees.cFee, fees.insFee), (c_fee, ins_fee));
    v.view(FEES, IFees::maxSwapFeeCall {}).await;
    v.view(FEES, IFees::liqFeeCall {}).await;
    v.view(FEES, IFees::lpFeeConfigCall {}).await;
    v.view(FEES, IFees::targetUtilizationCall {}).await;
    v.view(FEES, IFees::slopeBeforeTargetCall {}).await;
    v.view(FEES, IFees::slopeAfterTargetCall {}).await;
    let util = IFees::utilFeesCall {
        longUtilization: U256::ZERO,
        shortUtilization: U256::ZERO,
    };
    v.view(FEES, util).await;
    let funding = IFunding::fundingCall {
        spots: spots.clone(),
        emas: emas.clone(),
        markPrice: rates.lastMark,
    };
    v.view(FUNDING, funding).await;
    v.view(FUNDING, IFunding::fundingRateMultiplierCall {})
        .await;
    v.view(FUNDING, IFunding::maxFundingPerDayPercentCall {})
        .await;
    let ratios = v
        .view(MARGIN_RATIOS, IMarginRatios::makerMarginRatiosCall {})
        .await;
    assert_eq!(
        ratios.init.to::<u32>(),
        1_000_000,
        "the Sepolia maker initial ratio (W11 fact 2)"
    );
    let stored = v
        .view(MARGIN_RATIOS, IMarginRatios::makerRatiosCall {})
        .await;
    assert_eq!(
        (stored.init, stored.liq, stored.backstop),
        (ratios.init, ratios.liq, ratios.backstop)
    );
    let ratios = v
        .view(MARGIN_RATIOS, IMarginRatios::takerMarginRatiosCall {})
        .await;
    let stored = v
        .view(MARGIN_RATIOS, IMarginRatios::takerRatiosCall {})
        .await;
    assert_eq!(
        (stored.init, stored.liq, stored.backstop),
        (ratios.init, ratios.liq, ratios.backstop)
    );
    v.view(MARGIN_RATIOS, IMarginRatios::pendingMakerRatiosCall {})
        .await;
    v.view(MARGIN_RATIOS, IMarginRatios::pendingTakerRatiosCall {})
        .await;
    v.view(MARGIN_RATIOS, IMarginRatios::UPDATE_DELAYCall {})
        .await;
    let amm = U256::from(spots.ammPrice);
    let bounds = IPriceImpact::sqrtPriceBoundsCall {
        ammPrice: amm,
        index: U256::from(index),
        emaAmmPrice: amm,
        emaIndex: U256::from(index),
        markPrice: rates.lastMark,
    };
    let bounds = v.view(PRICE_IMPACT, bounds).await;
    assert!(bounds.sqrtMin < bounds.sqrtMax);
    v.view(PRICE_IMPACT, IPriceImpact::priceImpactBoundCall {})
        .await;
    let fair = IPricing::fairPriceCall {
        ammPrice: amm,
        index: U256::from(index),
        emaAmmPrice: amm,
        emaIndex: U256::from(index),
    };
    assert!(v.view(PRICING, fair).await > U256::ZERO);
    v.view(PRICING, IPricing::maxPremiumRatioCall {}).await;

    // The beacon.
    let beacon = v.view(m.beacon, IBeacon::indexCall {}).await;
    assert_eq!(beacon.index, v1_fork::index_2());
    assert!(beacon.updatedAt > U256::ZERO);
    v.view(m.beacon, IBeacon::twAvgCall { secondsAgo: 0 }).await;
}

/// The typed reverts of three refusals decode by the `v1` error binding.
async fn check_reverts(v: &Views<'_>) {
    let (w, m) = (v.w, v.m);
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
    let maker = v
        .view(m.perp, Perp::makerDetailsCall { posId: m.maker_id })
        .await;
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
