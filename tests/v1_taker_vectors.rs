//! W12 `taker_fees.json` kinds for the `v1.0.0` taker writes
//! (`fixtures/w12-taker-fees.json`, from `generate_w12_taker_fees.py`):
//! the fee bound (`max_total_fee_cap`, 32), the slippage limit
//! (`taker_slippage_limit`, 30), the swap walk (`swap_fill`, 7) and the fee
//! legs of the eight-field `SwapResult` (`swap_fees`, 7). Each kind loads
//! exactly its count.
use std::collections::BTreeMap;
use std::str::FromStr;

use alloy::primitives::{Address, I256, U256};
use perpcity_sdk::contracts::v1::{Perp, SwapResult};
use perpcity_sdk::events::receipt::V1PositionEvent;
use perpcity_sdk::{
    BlockContext, Era, LDelta, LUnits, MAX_TOTAL_FEE_SLACK, PerpDelta, PoolSnapshot, QuoteLimit,
    Ratio, SqrtPrice, TakerFeeRates, TakerQuote, TickLiquidity, UsdcAtoms, UsdcDelta,
};
use serde_json::Value;

fn cases(kind: &str, count: usize) -> Vec<Value> {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/w12-taker-fees.json")).unwrap();
    let cases: Vec<Value> = rows.into_iter().filter(|r| r["kind"] == kind).collect();
    assert_eq!(cases.len(), count, "{kind} cases loaded");
    cases
}

fn num<T: FromStr>(v: &Value) -> T
where
    T::Err: std::fmt::Debug,
{
    v.as_str().unwrap().parse().unwrap()
}

/// A `BalanceDelta`: `amount0` in the high 128 bits, `amount1` in the low.
fn pack_balance_delta(amount0: i128, amount1: i128) -> I256 {
    I256::from_raw((U256::from(amount0 as u128) << 128) | U256::from(amount1 as u128))
}

fn ratio(v: &Value) -> Ratio {
    Ratio::from_e6(num(v)).unwrap()
}

/// `maxTotalFeeFor` to the atom, and the vector's `cap_holds`: the bound is
/// at least the fee the scenario's trade paid.
#[test]
fn max_total_fee_cap() {
    let cases = cases("max_total_fee_cap", 32);
    let mut checked_holds = 0;
    for case in &cases {
        let i = &case["inputs"];
        let rates = TakerFeeRates {
            max_swap_fee: ratio(&i["max_swap_fee_e6"]),
            protocol_fee: ratio(&i["protocol_fee_e6"]),
        };
        let volume: i128 = num(&i["usd_volume"]);
        let slack = UsdcAtoms::new(num(&i["rounding_slack"]));
        let fee = rates
            .max_total_fee(UsdcAtoms::new(volume.unsigned_abs()), slack)
            .unwrap();
        assert_eq!(
            fee.atoms(),
            num::<u128>(&case["expected"]["max_total_fee"]),
            "{}",
            case["id"]
        );
        if let Some(holds) = case["intermediate"]["cap_holds"].as_bool() {
            let paid: u128 = num(&i["actual_total_fee_of_trade"]);
            assert_eq!(fee.atoms() >= paid, holds, "{}", case["id"]);
            checked_holds += 1;
        }
    }
    assert!(checked_holds > 0);
}

fn quote_for(usd_delta: i128, perp_delta: i128) -> TakerQuote {
    let price = SqrtPrice::from_x96(U256::from(1u8) << 96);
    TakerQuote {
        requested_perp_delta: PerpDelta::new(perp_delta),
        perp_delta: PerpDelta::new(perp_delta),
        usd_delta: UsdcDelta::new(usd_delta),
        sqrt_price_start: price,
        sqrt_price_after: price,
        tick_after: 0,
        liquidity_after: LUnits::ZERO,
        ticks_crossed: Vec::new(),
        fully_filled: true,
        price_impact_allowed: true,
        limit: QuoteLimit::Filled,
        block: BlockContext::default(),
    }
}

/// Decision 5c: a buy's limit keeps the ceiling, at most one atom above the
/// vector's floor; a sell's limit is the floor, exact.
#[test]
fn taker_slippage_limit() {
    let mut ceiled = 0;
    for case in cases("taker_slippage_limit", 30) {
        let i = &case["inputs"];
        let long = i["is_long"].as_bool().unwrap();
        let usd: i128 = num(&i["usd_delta"]);
        let quote = quote_for(usd, if long { 1 } else { -1 });
        let limit = quote.amt1_limit(num(&i["slippage_bps"])).atoms();
        let expected: u128 = num(&case["expected"]["amt1_limit"]);
        if long {
            assert!(
                limit == expected || limit == expected + 1,
                "{}: {limit} vs {expected}",
                case["id"]
            );
            ceiled += usize::from(limit != expected);
        } else {
            assert_eq!(limit, expected, "{}", case["id"]);
        }
    }
    assert!(ceiled > 0, "the ceiling is one atom above some floors");
}

/// The local V4 walk on the `v1.0.0` swap limits reproduces the
/// scenarios' fills: amounts, the price after, and no tick crossed.
#[test]
fn swap_fill() {
    for case in cases("swap_fill", 7) {
        let i = &case["inputs"];
        let ticks: BTreeMap<i32, TickLiquidity> = i["tick_map"]["ticks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                let net: i128 = num(&t[1]);
                (
                    num(&t[0]),
                    TickLiquidity {
                        gross: LUnits::new(net.unsigned_abs()),
                        net: LDelta::new(net),
                    },
                )
            })
            .collect();
        let (min, max) = Era::V1.swap_sqrt_price_limits();
        let pool = PoolSnapshot {
            block: BlockContext::default(),
            sqrt_price: SqrtPrice::from_x96(num(&i["sqrt_price_x96"])),
            tick: num(&i["tick"]),
            liquidity: LUnits::new(num(&i["liquidity"])),
            ticks,
            protocol_sqrt_min: min,
            protocol_sqrt_max: max,
            impact_sqrt_min: min,
            impact_sqrt_max: max,
        };
        let quote = pool
            .quote_perp(PerpDelta::new(num(&i["perp_delta"])))
            .unwrap();
        let e = &case["expected"];
        assert_eq!(
            quote.perp_delta.atoms(),
            num::<i128>(&e["amount0"]),
            "{}",
            case["id"]
        );
        assert_eq!(
            quote.usd_delta.atoms(),
            num::<i128>(&e["amount1"]),
            "{}",
            case["id"]
        );
        assert_eq!(
            quote.sqrt_price_after.x96(),
            num::<U256>(&e["sqrt_price_after_x96"]),
            "{}",
            case["id"]
        );
        assert_eq!(
            quote.ticks_crossed.len(),
            num::<usize>(&case["intermediate"]["ticks_crossed"]),
            "{}",
            case["id"]
        );
        assert!(quote.fully_filled);
    }
}

/// The fee legs the scenarios paid: protocol, creator and insurance each
/// rounded up from the volume, the LP leg from its segment rate, and the
/// total their sum. The total is within the bound the write sends for the
/// same volume at the deployed `maxSwapFee()` (8600), and a `TakerOpened`
/// with these legs in its eight-field `SwapResult` reports the deposit less
/// the total as the margin.
#[test]
fn swap_fees() {
    let up = |volume: u128, e6: u128| (volume * e6).div_ceil(1_000_000);
    for case in cases("swap_fees", 7) {
        let i = &case["inputs"];
        let e = &case["expected"];
        let volume: u128 = num(&i["abs_amount1"]);
        let config = &i["fees_config"];
        let fee = |key: &str| num::<u128>(&e[key]);
        assert_eq!(
            fee("protocol_fee"),
            up(volume, num(&i["protocol_fee_e6"])),
            "{}",
            case["id"]
        );
        assert_eq!(
            fee("creator_fee"),
            up(volume, num(&config["c_fee"])),
            "{}",
            case["id"]
        );
        assert_eq!(
            fee("insurance_fee"),
            up(volume, num(&config["ins_fee"])),
            "{}",
            case["id"]
        );
        let segment: u128 = num(&case["intermediate"]["segment_volume"]);
        let lp_rate: u128 = num(&case["intermediate"]["lp_fee_rate_e6"]);
        assert_eq!(fee("lp_fee"), up(segment, lp_rate), "{}", case["id"]);
        let total = fee("lp_fee") + fee("protocol_fee") + fee("creator_fee") + fee("insurance_fee");
        assert_eq!(total, fee("total_fee"), "{}", case["id"]);

        let rates = TakerFeeRates {
            max_swap_fee: Ratio::from_e6(8_600).unwrap(),
            protocol_fee: ratio(&i["protocol_fee_e6"]),
        };
        let bound = rates
            .max_total_fee(UsdcAtoms::new(volume), MAX_TOTAL_FEE_SLACK)
            .unwrap();
        assert!(bound.atoms() >= total, "{}", case["id"]);

        let deposit: u128 = 10_000_000;
        let opened = Perp::TakerOpened {
            posId: U256::from(3u8),
            owner: Address::repeat_byte(0x77),
            margin: deposit,
            sr: SwapResult {
                delta: pack_balance_delta(400_000, -(volume as i128)),
                ammPrice: U256::ZERO,
                totalFeeAmt: I256::try_from(total).unwrap(),
                lpFeeAmt: U256::from(fee("lp_fee")),
                protocolFeeAmt: U256::from(fee("protocol_fee")),
                creatorFeeAmt: U256::from(fee("creator_fee")),
                insuranceFeeAmt: U256::from(fee("insurance_fee")),
                feeFundFeeAmt: U256::ZERO,
            },
            lastCumlFundingX96: I256::ZERO,
        };
        assert_eq!(
            opened.post_trade_margin().unwrap(),
            Some(UsdcAtoms::new(deposit - total)),
            "{}",
            case["id"]
        );
    }
}
