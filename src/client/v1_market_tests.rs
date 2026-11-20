//! The market reads by build, against a mocked node: a `v1.0.0` market is
//! read in its own shapes (`emaWindow()`, the module-asked mark, slot 23,
//! the five-argument bounds, the close capacity, the eight-word
//! cumulatives, the fees module's own views), an earlier build keeps its
//! path, and an unknown build is refused before any market view is read.

use alloy::primitives::{Address, B256, I256, Signed, U256, Uint};
use alloy::providers::bindings::IMulticall3 as Multicall;
use alloy::sol_types::SolCall;

use crate::constants::{
    MAX_SWAP_SQRT_PRICE_X96, MIN_SWAP_SQRT_PRICE_X96, SNAPSHOT_BLOCK_LAG,
    V1_MAX_SWAP_SQRT_PRICE_X96, V1_MIN_SWAP_SQRT_PRICE_X96,
};
use crate::contracts::{
    IBeacon, IFees, IMarginRatios, IPoolManagerState, IPricing, Perp, Rates, v1,
};
use crate::errors::{ContractError, PerpCityError};
use crate::math::capacity::Capacity;
use crate::math::pricing::{PricePair, calculate_emas, fair_price};
use crate::units::{
    Earnings, FeeGrowth, Funding, FundingCorrection, FundingPerSqrtPriceX192, LUnits, PerSide,
    PerpAtoms, Price, Ratio, Side, SqrtPrice,
};

use super::mock::{self, BEACON, Rpc, e6, returns, x96};
use super::queries::{Era, Fees, LpFee, LpFeeCurve};
use super::state::StateAt;
use super::{ARBITRUM_CHAIN_ID, ARBITRUM_SEPOLIA_CHAIN_ID, ChainDeployments};

const CHAINS: [(ChainDeployments, u64); 2] = [
    (ChainDeployments::ARBITRUM, ARBITRUM_CHAIN_ID),
    (
        ChainDeployments::ARBITRUM_SEPOLIA,
        ARBITRUM_SEPOLIA_CHAIN_ID,
    ),
];

fn immutables(rpc: &Rpc, chain: ChainDeployments, era: Era) {
    rpc.immutables_with_hooks(era, chain.v022_perp_guard_hook, chain.v1_perp_guard_hook);
}

/// The handle's block time.
const NOW: u64 = 1_700_000_600;
/// The market's last touch, one window before [`NOW`].
const TOUCHED: u64 = 1_700_000_000;
/// The EMA window both builds report.
const WINDOW: u32 = 600;
/// A hook no deployment names.
const UNKNOWN_HOOK: Address = Address::repeat_byte(0x0e);

/// The stored pair, the spots, and the pricing module's answer: an answer
/// no port of either rule gives for these inputs, so a read that took it
/// is the read that asked.
fn stored() -> PricePair {
    PricePair {
        amm: x96(51, 0).to::<u128>(),
        index: x96(50, 0).to::<u128>(),
    }
}
fn pool_price() -> U256 {
    x96(52, 0)
}
fn index() -> U256 {
    x96(55, 0)
}
fn module_mark() -> U256 {
    x96(56, 0) + U256::from(7u8)
}

fn v1_modules() -> v1::Modules {
    let m = mock::modules();
    v1::Modules {
        beacon: m.beacon,
        fees: m.fees,
        funding: m.funding,
        marginRatios: m.marginRatios,
        priceImpact: m.priceImpact,
        pricing: m.pricing,
    }
}

fn v1_pool_state() -> v1::Perp::poolStateReturn {
    v1::Perp::poolStateReturn {
        tick: Signed::ZERO,
        sqrtPrice: Uint::from(1u8) << 96,
        ammPrice: pool_price(),
        liquidity: 0,
    }
}

fn v1_rates() -> v1::Rates {
    v1::Rates {
        fundingPerDay: Signed::ZERO,
        longUtilFeePerDay: 0,
        shortUtilFeePerDay: 0,
        lastTouch: Uint::from(TOUCHED),
        lastMark: x96(50, 0),
    }
}

/// A handle at block 92, [`NOW`].
async fn handle(chain: ChainDeployments, chain_id: u64) -> (StateAt, Rpc) {
    let (client, rpc) = mock::client_on(chain, chain_id);
    rpc.block(92, NOW);
    let state = client.market().state_at(92).await.unwrap();
    (state, rpc)
}

/// The immutables of a market whose hook no deployment names.
fn unknown_build(rpc: &Rpc) {
    rpc.call::<Perp::poolKeyCall>(&mock::pool_key_with_hook(30, UNKNOWN_HOOK));
}

fn is_unknown_build(err: &PerpCityError) -> bool {
    matches!(
        err,
        PerpCityError::Contract(ContractError::UnknownBuild { hook, .. }) if *hook == UNKNOWN_HOOK
    )
}

fn is_unsupported(err: &PerpCityError, era: Era, operation: &str) -> bool {
    matches!(
        err,
        PerpCityError::Contract(ContractError::UnsupportedEra { era: e, operation: o })
            if *e == era && *o == operation
    )
}

/// A `v1.0.0` market's mark reads after the views multicall: the
/// market's accepted index, the slot-23 word, then the module's answer.
fn v1_mark_answers(rpc: &Rpc, block: u64) {
    rpc.aggregate(
        block,
        [
            returns::<v1::Perp::modulesCall>(&v1_modules()),
            returns::<v1::Perp::poolStateCall>(&v1_pool_state()),
            returns::<v1::Perp::ratesCall>(&v1_rates()),
            returns::<v1::Perp::emaWindowCall>(&e6(WINDOW)),
        ],
    );
    rpc.aggregate3(vec![mock::ok_row(
        returns::<v1::Perp::readBeaconIndexCall>(&index().to::<u128>()),
    )]);
    rpc.storage(mock::emas_word(stored().amm, stored().index));
    rpc.call::<v1::IPricing::fairPriceCall>(&module_mark());
}

/// The pair the contract advances to [`NOW`] from the stored one.
fn advanced() -> PricePair {
    let spot = PricePair::try_from_x96(pool_price(), index()).unwrap();
    calculate_emas(stored(), spot, TOUCHED, NOW, u64::from(WINDOW)).unwrap()
}

/// The selector of every `eth_call`, and of every call inside a
/// Multicall3 `aggregate` in its place, in order.
fn selectors(rpc: &Rpc) -> Vec<[u8; 4]> {
    let first4 = |data: &[u8]| -> [u8; 4] { data[..4].try_into().unwrap() };
    rpc.calls()
        .iter()
        .flat_map(
            |calldata| match Multicall::aggregateCall::abi_decode(calldata) {
                Ok(batch) => batch
                    .calls
                    .iter()
                    .map(|call| first4(&call.callData))
                    .collect(),
                Err(_) => vec![first4(calldata)],
            },
        )
        .collect()
}

/// The decoded calldata of the last `eth_call` of `C`'s selector.
fn last_call<C: SolCall>(rpc: &Rpc) -> C {
    let calldata = rpc
        .calls()
        .into_iter()
        .rev()
        .find(|c| c.starts_with(&C::SELECTOR))
        .expect("the call was asked");
    C::abi_decode(&calldata).unwrap()
}

// ── The mark (rows 11, 18, 19, 52) ────────────────────────────────────

/// A `v1.0.0` market's mark: `emaWindow()` in the views, the EMAs from
/// slot 23 advanced to the block, and the price the pricing module gives
/// for them, not a port of either rule.
#[tokio::test]
async fn a_v1_mark_is_the_pricing_modules_answer_for_the_advanced_emas() {
    for (chain, chain_id) in CHAINS {
        let (state, rpc) = handle(chain, chain_id).await;
        immutables(&rpc, chain, Era::V1);
        v1_mark_answers(&rpc, 92);

        let mark = state.mark().await.unwrap();
        assert_eq!(mark.block, state.block());
        assert_eq!(mark.pool_price, Price::from_x96(pool_price()));
        assert_eq!(mark.index, Price::from_x96(index()));
        assert_eq!(mark.emas, advanced());
        assert_ne!(mark.emas, stored(), "a window of time was advanced over");
        assert_eq!(mark.price, Price::from_x96(module_mark()));
        assert_eq!(mark.fair_price(), mark.price);
        assert_eq!(rpc.slots(), [U256::from(23u8)]);

        let asked = last_call::<v1::IPricing::fairPriceCall>(&rpc);
        assert_eq!(asked.ammPrice, pool_price());
        assert_eq!(asked.index, index());
        assert_eq!(asked.emaAmmPrice, U256::from(advanced().amm));
        assert_eq!(asked.emaIndex, U256::from(advanced().index));
        let selectors = selectors(&rpc);
        assert!(selectors.contains(&v1::Perp::emaWindowCall::SELECTOR));
        assert!(!selectors.contains(&Perp::EMA_WINDOWCall::SELECTOR));
        assert!(
            rpc.is_drained(),
            "immutables, views, index, slot, fairPrice"
        );
    }
}

/// The two earlier builds keep their path: `EMA_WINDOW()`, slot 11, the
/// ported rule, and no pricing module asked.
#[tokio::test]
async fn an_earlier_builds_mark_keeps_slot_11_and_the_port() {
    for (chain, chain_id) in CHAINS {
        for era in [Era::Legacy, Era::Upgradeable] {
            let (state, rpc) = handle(chain, chain_id).await;
            immutables(&rpc, chain, era);
            rpc.aggregate(
                92,
                [
                    returns::<Perp::modulesCall>(&mock::modules()),
                    returns::<Perp::poolStateCall>(&mock::pool_state(pool_price())),
                    returns::<Perp::ratesCall>(&Rates {
                        lastTouch: Uint::from(TOUCHED),
                        ..mock::rates(0)
                    }),
                    returns::<Perp::EMA_WINDOWCall>(&U256::from(WINDOW)),
                ],
            );
            rpc.call::<IBeacon::indexCall>(&index());
            rpc.storage(mock::emas_word(stored().amm, stored().index));

            let mark = state.mark().await.unwrap();
            assert_eq!(mark.emas, advanced());
            assert_eq!(
                mark.price,
                fair_price(
                    Price::from_x96(pool_price()),
                    Price::from_x96(index()),
                    Price::from_x96(U256::from(advanced().amm)),
                    Price::from_x96(U256::from(advanced().index)),
                ),
                "{era:?}"
            );
            assert_eq!(rpc.slots(), [U256::from(11u8)]);
            assert!(!selectors(&rpc).contains(&IPricing::fairPriceCall::SELECTOR));
            assert!(rpc.is_drained(), "{era:?}");
        }
    }
}

/// An unknown build is refused after its pool key, before any view; the
/// refusal is not cached, so each read asks the key again.
#[tokio::test]
async fn every_market_read_refuses_an_unknown_build() {
    for (chain, chain_id) in CHAINS {
        let (state, rpc) = handle(chain, chain_id).await;
        unknown_build(&rpc);
        assert!(is_unknown_build(&state.mark().await.unwrap_err()));
        unknown_build(&rpc);
        assert!(is_unknown_build(&state.emas().await.unwrap_err()));
        unknown_build(&rpc);
        assert!(is_unknown_build(&state.capacity().await.unwrap_err()));
        unknown_build(&rpc);
        assert!(is_unknown_build(&state.cumulatives().await.unwrap_err()));
        unknown_build(&rpc);
        assert!(is_unknown_build(&state.v1_cumulatives().await.unwrap_err()));
        unknown_build(&rpc);
        assert!(is_unknown_build(&state.beacon_index().await.unwrap_err()));
        unknown_build(&rpc);
        assert!(is_unknown_build(&state.pool().await.unwrap_err()));
        unknown_build(&rpc);
        let market = state.market();
        assert!(is_unknown_build(&market.get_config().await.unwrap_err()));
        unknown_build(&rpc);
        assert!(is_unknown_build(&market.get_snapshot().await.unwrap_err()));
        assert!(rpc.is_drained(), "one pool key per read, nothing else");
        assert!(
            selectors(&rpc)
                .iter()
                .all(|s| *s == Perp::poolKeyCall::SELECTOR)
        );
    }
}

// ── The stored EMAs (row 18) ──────────────────────────────────────────

/// The stored pair comes from the build's slot, the touch from `rates()`.
#[tokio::test]
async fn emas_read_the_builds_slot() {
    for (chain, chain_id) in CHAINS {
        for (era, slot) in [(Era::Legacy, 11u8), (Era::V1, 23)] {
            let (state, rpc) = handle(chain, chain_id).await;
            immutables(&rpc, chain, era);
            rpc.call::<Perp::ratesCall>(&Rates {
                lastTouch: Uint::from(TOUCHED),
                ..mock::rates(0)
            });
            rpc.storage(mock::emas_word(stored().amm, stored().index));

            let emas = state.emas().await.unwrap();
            assert_eq!(emas.pair().unwrap(), stored(), "{era:?}");
            assert_eq!(emas.last_touch, TOUCHED);
            assert_eq!(rpc.slots(), [U256::from(slot)], "{era:?}");
            assert!(rpc.is_drained(), "{era:?}");
        }
    }
}

// ── The pool and its bounds (rows 20, 21, 60) ─────────────────────────

/// A `v1.0.0` pool asks the price-impact module with the mark as its
/// fifth argument and stops at the build's own swap limits.
#[tokio::test]
async fn a_v1_pool_takes_the_five_argument_bounds_and_its_swap_limits() {
    for (chain, chain_id) in CHAINS {
        let (state, rpc) = handle(chain, chain_id).await;
        immutables(&rpc, chain, Era::V1);
        v1_mark_answers(&rpc, 92);
        let one = x96(1, 0);
        rpc.call::<v1::IPriceImpact::sqrtPriceBoundsCall>(
            &v1::IPriceImpact::sqrtPriceBoundsReturn {
                sqrtMin: one >> 1,
                sqrtMax: one << 1,
            },
        );
        rpc.call::<IPoolManagerState::extsload_1Call>(&vec![B256::ZERO; 36]);

        let pool = state.pool().await.unwrap();
        assert_eq!(
            (pool.protocol_sqrt_min, pool.protocol_sqrt_max),
            (
                SqrtPrice::from_x96(V1_MIN_SWAP_SQRT_PRICE_X96),
                SqrtPrice::from_x96(V1_MAX_SWAP_SQRT_PRICE_X96)
            )
        );
        assert_eq!(pool.impact_sqrt_min, SqrtPrice::from_x96(one >> 1));
        assert_eq!(pool.impact_sqrt_max, SqrtPrice::from_x96(one << 1));
        let asked = last_call::<v1::IPriceImpact::sqrtPriceBoundsCall>(&rpc);
        assert_eq!(asked.markPrice, module_mark());
        assert_eq!(asked.emaAmmPrice, U256::from(advanced().amm));
        assert!(rpc.is_drained());
    }
}

/// The limits by build: Uniswap's on the earlier two, the maker band's
/// edges on `v1.0.0`; and the protocol fee caps, 5% and 1%.
#[test]
fn the_swap_limits_and_the_protocol_fee_cap_follow_the_build() {
    for era in [Era::Legacy, Era::Upgradeable] {
        assert_eq!(
            era.swap_sqrt_price_limits(),
            (
                SqrtPrice::from_x96(MIN_SWAP_SQRT_PRICE_X96),
                SqrtPrice::from_x96(MAX_SWAP_SQRT_PRICE_X96)
            )
        );
        assert_eq!(era.max_protocol_fee(), Ratio::from_e6(50_000).unwrap());
    }
    assert_eq!(
        Era::V1.swap_sqrt_price_limits(),
        (
            SqrtPrice::from_x96(V1_MIN_SWAP_SQRT_PRICE_X96),
            SqrtPrice::from_x96(V1_MAX_SWAP_SQRT_PRICE_X96)
        )
    );
    assert_eq!(Era::V1.max_protocol_fee(), Ratio::from_e6(10_000).unwrap());
}

// ── Capacity (rows 22, 56) ────────────────────────────────────────────

/// A `v1.0.0` market's capacity batch carries the close depth, and its
/// utilization is the build's: full at no capacity, capped at 100%.
#[tokio::test]
async fn v1_capacity_carries_the_close_depth_and_the_capped_utilization() {
    for (chain, chain_id) in CHAINS {
        let (state, rpc) = handle(chain, chain_id).await;
        immutables(&rpc, chain, Era::V1);
        rpc.aggregate(
            92,
            [
                returns::<v1::Perp::capacityCall>(&v1::Capacity {
                    long: 0,
                    short: 400,
                }),
                returns::<v1::Perp::openInterestCall>(&v1::OpenInterest {
                    long: 300,
                    short: 500,
                }),
                returns::<v1::Perp::physicalCloseCapacityCall>(
                    &v1::Perp::physicalCloseCapacityReturn {
                        longClose: 900,
                        shortClose: 1_000,
                    },
                ),
            ],
        );

        let capacity = state.capacity().await.unwrap();
        assert_eq!(capacity.block, state.block());
        assert_eq!(
            capacity.close_capacity,
            Some(Capacity::new(PerpAtoms::new(900), PerpAtoms::new(1_000)))
        );
        assert_eq!(capacity.utilization_e6(Side::Long), Some(1_000_000));
        assert_eq!(capacity.utilization_e6(Side::Short), Some(1_000_000));
        assert_eq!(
            selectors(&rpc),
            [
                Perp::poolKeyCall::SELECTOR,
                v1::Perp::poolIdCall::SELECTOR,
                v1::Perp::capacityCall::SELECTOR,
                v1::Perp::openInterestCall::SELECTOR,
                v1::Perp::physicalCloseCapacityCall::SELECTOR,
            ]
        );
        assert!(rpc.is_drained());
    }
}

// ── Cumulatives (row 17) ──────────────────────────────────────────────

/// A `v1.0.0` market's eight words become their own type, and neither
/// read decodes the other build's: each refuses it after the era alone.
#[tokio::test]
async fn cumulatives_are_read_only_in_the_builds_own_type() {
    for (chain, chain_id) in CHAINS {
        let (state, rpc) = handle(chain, chain_id).await;
        immutables(&rpc, chain, Era::V1);
        let err = state.cumulatives().await.unwrap_err();
        assert!(is_unsupported(&err, Era::V1, "cumulatives"), "{err}");
        assert!(!err.is_transient());
        assert!(rpc.is_drained(), "the era and nothing else");

        let words = v1::Cumulatives {
            fundingX96: I256::try_from(-7).unwrap(),
            fundingDivSqrtPX192: I256::try_from(11).unwrap(),
            lpFeeGrowthGlobalX128: U256::from(13u8),
            longUtilPaymentsX96: U256::from(17u8),
            shortUtilPaymentsX96: U256::from(19u8),
            longReserveUtilPaymentsX96: U256::from(23u8),
            shortReserveUtilPaymentsX96: U256::from(29u8),
            fundingCorrectionX128: I256::try_from(-31).unwrap(),
        };
        rpc.call::<v1::Perp::cumulativesCall>(&words);
        let read = state.v1_cumulatives().await.unwrap();
        assert_eq!(read.funding, Funding::from_x96(I256::try_from(-7).unwrap()));
        assert_eq!(
            read.funding_div_sqrt_p,
            FundingPerSqrtPriceX192::from_x192(I256::try_from(11).unwrap())
        );
        assert_eq!(read.lp_fee_growth, FeeGrowth::from_x128(U256::from(13u8)));
        assert_eq!(
            read.util_payments,
            PerSide::new(
                Earnings::from_x96(U256::from(17u8)),
                Earnings::from_x96(U256::from(19u8))
            )
        );
        assert_eq!(
            read.reserve_util_payments,
            PerSide::new(
                Earnings::from_x96(U256::from(23u8)),
                Earnings::from_x96(U256::from(29u8))
            )
        );
        assert_eq!(
            read.funding_correction,
            FundingCorrection::from_x128(I256::try_from(-31).unwrap())
        );
        assert!(rpc.is_drained());

        let (state, rpc) = handle(chain, chain_id).await;
        immutables(&rpc, chain, Era::Upgradeable);
        let err = state.v1_cumulatives().await.unwrap_err();
        assert!(
            is_unsupported(&err, Era::Upgradeable, "v1_cumulatives"),
            "{err}"
        );
        assert!(rpc.is_drained());
    }
}

// ── The beacon's index (row 23) ───────────────────────────────────────

/// A `v1.0.0` market's index read: the modules and the latency in one
/// batch, then the beacon's value and time; refused on an earlier build,
/// whose beacon has no time to give.
#[tokio::test]
async fn the_beacon_index_carries_its_time_and_the_markets_latency() {
    for (chain, chain_id) in CHAINS {
        let (state, rpc) = handle(chain, chain_id).await;
        immutables(&rpc, chain, Era::V1);
        rpc.aggregate(
            92,
            [
                returns::<v1::Perp::modulesCall>(&v1_modules()),
                returns::<v1::Perp::latencyCall>(&3_600),
            ],
        );
        let observation = v1::IBeacon::indexReturn {
            index: index(),
            updatedAt: U256::from(NOW - 3_600),
        };
        rpc.call::<v1::IBeacon::indexCall>(&observation);

        let reading = state.beacon_index().await.unwrap();
        assert_eq!(reading.block, state.block());
        assert_eq!(reading.index, Price::from_x96(index()));
        assert_eq!(reading.updated_at, NOW - 3_600);
        assert_eq!(reading.latency, 3_600);
        assert!(!reading.is_stale_at(NOW), "exactly the latency is accepted");
        assert!(reading.is_stale_at(NOW + 1));
        assert_eq!(reading.accepted_until(), NOW);
        assert!(reading.is_in_domain());
        assert_eq!(
            rpc.selectors().last(),
            Some(&v1::IBeacon::indexCall::SELECTOR)
        );
        assert!(rpc.is_drained());

        let (state, rpc) = handle(chain, chain_id).await;
        immutables(&rpc, chain, Era::Legacy);
        let err = state.beacon_index().await.unwrap_err();
        assert!(is_unsupported(&err, Era::Legacy, "beacon_index"), "{err}");
        assert!(rpc.is_drained());
    }
}

// ── Configuration and the snapshot (rows 9, 10, 11) ───────────────────

/// The `v1.0.0` fees module's own views, in one batch: the rates, the LP
/// curve and the swap-fee cap, and the liquidation fee.
fn v1_fees_answers(rpc: &Rpc) {
    rpc.aggregate(
        0,
        [
            returns::<v1::IFees::cFeeCall>(&e6(100)),
            returns::<v1::IFees::insFeeCall>(&e6(1_000)),
            returns::<v1::IFees::lpFeeConfigCall>(&v1::IFees::lpFeeConfigReturn {
                startFee: e6(5_000),
                targetFee: e6(100),
                liquidityDecay: 15_000_000,
                volatilityScaler: e6(200_000),
                maxFeeMultiplier: e6(1_500_000),
            }),
            returns::<v1::IFees::maxSwapFeeCall>(&U256::from(8_600u32)),
            returns::<v1::IFees::liqFeeCall>(&e6(5_000)),
        ],
    );
    rpc.call::<IMarginRatios::takerMarginRatiosCall>(&IMarginRatios::takerMarginRatiosReturn {
        init: e6(100_000),
        liq: e6(70_000),
        backstop: e6(40_000),
    });
}

fn ratio(e6: u32) -> Ratio {
    Ratio::from_e6(e6).unwrap()
}

fn v1_fees() -> Fees {
    Fees {
        creator_fee: ratio(100),
        insurance_fee: ratio(1_000),
        lp_fee: LpFee::Curve(LpFeeCurve {
            start_fee: ratio(5_000),
            target_fee: ratio(100),
            liquidity_decay: LUnits::new(15_000_000),
            volatility_scaler: ratio(200_000),
            max_fee_multiplier: ratio(1_500_000),
            max_swap_fee: ratio(8_600),
        }),
        liquidation_fee: ratio(5_000),
    }
}

/// `get_config` on `v1.0.0`: the window from `emaWindow()` and the fees
/// from the module's own views; no zero-argument `fees()` is asked.
#[tokio::test]
async fn v1_config_reads_the_window_and_the_fee_curve() {
    for (chain, chain_id) in CHAINS {
        let (client, rpc) = mock::client_on(chain, chain_id);
        immutables(&rpc, chain, Era::V1);
        rpc.call::<Perp::modulesCall>(&mock::modules());
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key_with_hook(30, chain.v1_perp_guard_hook));
        rpc.call::<Perp::poolStateCall>(&mock::pool_state(pool_price()));
        rpc.call::<v1::Perp::emaWindowCall>(&e6(WINDOW));
        v1_fees_answers(&rpc);

        let config = client.market().get_config().await.unwrap();
        assert_eq!(config.ema_window, u64::from(WINDOW));
        assert_eq!(config.fees, v1_fees());
        assert_eq!(config.beacon, BEACON);
        let selectors = selectors(&rpc);
        assert!(!selectors.contains(&IFees::feesCall::SELECTOR));
        assert!(!selectors.contains(&Perp::EMA_WINDOWCall::SELECTOR));
        assert!(rpc.is_drained());
    }
}

/// `get_snapshot` on `v1.0.0`: one batch at the lagged block in the
/// build's shapes, the index and slot 23, then the module's mark.
#[tokio::test]
async fn a_v1_snapshot_marks_by_the_pricing_module_at_the_lagged_block() {
    for (chain, chain_id) in CHAINS {
        let (client, rpc) = mock::client_on(chain, chain_id);
        immutables(&rpc, chain, Era::V1);
        rpc.quantity(100);
        let lagged = 100 - SNAPSHOT_BLOCK_LAG;
        let hash = rpc.block(lagged, NOW);
        rpc.aggregate(
            lagged,
            [
                returns::<v1::Perp::modulesCall>(&v1_modules()),
                returns::<v1::Perp::poolKeyCall>(&v1::PoolKey {
                    currency0: Address::ZERO,
                    currency1: mock::USDC,
                    fee: Uint::ZERO,
                    tickSpacing: Signed::try_from(30).unwrap(),
                    hooks: chain.v1_perp_guard_hook,
                }),
                returns::<v1::Perp::poolStateCall>(&v1_pool_state()),
                returns::<v1::Perp::ratesCall>(&v1_rates()),
                returns::<v1::Perp::openInterestCall>(&v1::OpenInterest { long: 3, short: 4 }),
                returns::<v1::Perp::emaWindowCall>(&e6(WINDOW)),
            ],
        );
        rpc.aggregate3(vec![mock::ok_row(
            returns::<v1::Perp::readBeaconIndexCall>(&index().to::<u128>()),
        )]);
        rpc.storage(mock::emas_word(stored().amm, stored().index));
        rpc.call::<v1::IPricing::fairPriceCall>(&module_mark());
        v1_fees_answers(&rpc);

        let (config, snapshot) = client.market().get_snapshot().await.unwrap();
        assert_eq!(snapshot.block.number, lagged);
        assert_eq!(snapshot.block.hash, hash);
        assert_eq!(snapshot.mark, Price::from_x96(module_mark()));
        assert_eq!(snapshot.index_price, Price::from_x96(index()));
        assert_eq!(snapshot.pool_price, Price::from_x96(pool_price()));
        assert_eq!(snapshot.emas.pair().unwrap(), stored());
        assert_eq!(snapshot.emas.last_touch, TOUCHED);
        assert_eq!(
            snapshot.open_interest,
            PerSide::new(PerpAtoms::new(3), PerpAtoms::new(4))
        );
        assert_eq!(config.ema_window, u64::from(WINDOW));
        assert_eq!(config.tick_spacing, 30);
        assert_eq!(config.fees, v1_fees());
        assert_eq!(rpc.slots(), [U256::from(23u8)]);
        let asked = last_call::<v1::IPricing::fairPriceCall>(&rpc);
        assert_eq!(asked.emaIndex, U256::from(advanced().index));
        assert!(rpc.is_drained());
    }
}

// ── The views both builds answer alike ────────────────────────────────

/// `get_config` reads `modules()`, `poolKey()` and `poolState()` and
/// `emas()` reads `rates()` through the earlier builds' binding on every
/// build: the selectors are the same, and a `v1.0.0` answer decodes in
/// that binding to the same words (`rates()`'s fifth word, `lastMark`,
/// is past the four it reads).
#[test]
fn the_views_read_through_the_shared_binding_decode_a_v1_answer() {
    assert_eq!(Perp::modulesCall::SELECTOR, v1::Perp::modulesCall::SELECTOR);
    assert_eq!(Perp::poolKeyCall::SELECTOR, v1::Perp::poolKeyCall::SELECTOR);
    assert_eq!(
        Perp::poolStateCall::SELECTOR,
        v1::Perp::poolStateCall::SELECTOR
    );
    assert_eq!(Perp::ratesCall::SELECTOR, v1::Perp::ratesCall::SELECTOR);

    let modules =
        Perp::modulesCall::abi_decode_returns(&returns::<v1::Perp::modulesCall>(&v1_modules()))
            .unwrap();
    assert_eq!(
        returns::<Perp::modulesCall>(&modules),
        returns::<Perp::modulesCall>(&mock::modules())
    );

    let state = v1_pool_state();
    let shared =
        Perp::poolStateCall::abi_decode_returns(&returns::<v1::Perp::poolStateCall>(&state))
            .unwrap();
    assert_eq!(
        (
            shared.tick,
            shared.sqrtPrice,
            shared.ammPrice,
            shared.liquidity
        ),
        (state.tick, state.sqrtPrice, state.ammPrice, state.liquidity)
    );

    let rates = v1::Rates {
        fundingPerDay: Signed::try_from(-5).unwrap(),
        longUtilFeePerDay: 6,
        shortUtilFeePerDay: 7,
        ..v1_rates()
    };
    let shared =
        Perp::ratesCall::abi_decode_returns(&returns::<v1::Perp::ratesCall>(&rates)).unwrap();
    assert_eq!(
        (
            shared.fundingPerDay,
            shared.longUtilFeePerDay,
            shared.shortUtilFeePerDay,
            shared.lastTouch
        ),
        (
            rates.fundingPerDay,
            rates.longUtilFeePerDay,
            rates.shortUtilFeePerDay,
            rates.lastTouch
        )
    );

    let key = v1::PoolKey {
        currency0: Address::repeat_byte(1),
        currency1: mock::USDC,
        fee: Uint::ZERO,
        tickSpacing: Signed::try_from(30).unwrap(),
        hooks: mock::V1_HOOK,
    };
    let shared =
        Perp::poolKeyCall::abi_decode_returns(&returns::<v1::Perp::poolKeyCall>(&key)).unwrap();
    assert_eq!(
        (
            shared.currency0,
            shared.currency1,
            shared.tickSpacing,
            shared.hooks
        ),
        (key.currency0, key.currency1, key.tickSpacing, key.hooks)
    );
}
