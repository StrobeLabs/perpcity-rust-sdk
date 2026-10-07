//! The law a replay rests on, checked across random tapes rather than one:
//! the fold of a concatenation is the combination of the folds, at any cut,
//! so a fold of any prefix is a checkpoint and segments fold on separate
//! cores.

use std::collections::BTreeSet;

use alloy::primitives::{Address, B256, I256, U256};
use perpcity_sdk::events::{CumulativesInfo, MarketEvent, ModuleKind};
use perpcity_sdk::history::test_support::tape::{
    per_side, pool_initialized, pool_swapped, price, row, settle, swap,
};
use perpcity_sdk::history::{Fold, Replay, Tape, TapeEvent};
use perpcity_sdk::{
    Earnings, Funding, FundingPerSqrtPrice, FundingRate, LDelta, LUnits, PerSide, PerpAtoms,
    UsdcAtoms, UsdcDelta, UtilizationRate,
};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;

/// A position id from a small pool, so each position sees many events.
fn pos_id() -> impl Strategy<Value = U256> {
    (1u64..50).prop_map(U256::from)
}

/// A tick on the pool's spacing.
fn tick() -> impl Strategy<Value = i32> {
    (-10i32..10).prop_map(|t| t * 60)
}

fn cumulatives(n: u64) -> CumulativesInfo {
    let x96 = |k: u64| I256::try_from(n * k).unwrap();
    CumulativesInfo {
        funding: Funding::from_x96(x96(1)),
        funding_div_sqrt_p: FundingPerSqrtPrice::from_x96(x96(2)),
        util_payments: PerSide::new(
            Earnings::from_x96(U256::from(n * 3)),
            Earnings::from_x96(U256::from(n * 4)),
        ),
        util_earnings: PerSide::new(
            Earnings::from_x96(U256::from(n * 5)),
            Earnings::from_x96(U256::from(n * 6)),
        ),
    }
}

/// One of the events the fold reads, with the values a market could emit:
/// the market's totals, the positions' lives, and the pool's liquidity.
/// Every variant of the vocabulary is here; `name_of` holds it to that.
fn event() -> impl Strategy<Value = MarketEvent> {
    prop_oneof![
        (pos_id(), 1u64..1_000, 0u128..1_000_000).prop_map(|(pos_id, p, fee)| {
            MarketEvent::TakerOpened {
                pos_id,
                swap: swap(1_000_000, price(p), fee),
            }
        }),
        (pos_id(), -2_000_000i128..2_000_000, 1u64..1_000).prop_map(|(pos_id, d, p)| {
            MarketEvent::TakerAdjusted {
                pos_id,
                swap: swap(d, price(p), 0),
                funding: UsdcDelta::ZERO,
                util_fees: UsdcAtoms::ZERO,
            }
        }),
        (
            pos_id(),
            -2_000_000i128..2_000_000,
            1u64..1_000,
            any::<bool>()
        )
            .prop_map(|(pos_id, d, p, liquidated)| MarketEvent::TakerClosed {
                pos_id,
                swap: swap(d, price(p), 0),
                funding: UsdcDelta::ZERO,
                util_fees: UsdcAtoms::ZERO,
                liquidation_fee: UsdcAtoms::ZERO,
                is_liquidation: liquidated,
            }),
        (pos_id(), 1u128..1_000_000).prop_map(|(pos_id, amount)| MarketEvent::TakerLiquidated {
            pos_id,
            perp_amount: PerpAtoms::new(amount),
            liquidation_fee: UsdcAtoms::new(1),
        }),
        (pos_id(), -10i32..10, 1i32..10, -1_000i128..1_000).prop_map(
            |(pos_id, lower, width, delta)| MarketEvent::ModifyLiquidity {
                pool_id: B256::with_last_byte(7),
                sender: Address::ZERO,
                tick_lower: lower * 60,
                tick_upper: (lower + width) * 60,
                liquidity_delta: LDelta::new(delta),
                salt: B256::from(pos_id),
            }
        ),
        tick().prop_map(pool_initialized),
        tick().prop_map(pool_swapped),
        pos_id().prop_map(|pos_id| MarketEvent::MakerOpened { pos_id }),
        pos_id().prop_map(|pos_id| MarketEvent::MakerAdjusted {
            pos_id,
            settle: settle(),
        }),
        (pos_id(), any::<bool>()).prop_map(|(pos_id, liquidated)| MarketEvent::MakerConverted {
            pos_id,
            settle: settle(),
            liquidation_fee: UsdcAtoms::ZERO,
            is_liquidation: liquidated,
        }),
        (pos_id(), any::<bool>()).prop_map(|(pos_id, liquidated)| MarketEvent::MakerClosed {
            pos_id,
            settle: settle(),
            liquidation_fee: UsdcAtoms::ZERO,
            is_liquidation: liquidated,
        }),
        (pos_id(), 1u128..1_000).prop_map(|(pos_id, amount)| MarketEvent::MakerLiquidated {
            pos_id,
            liquidity_amount: LUnits::new(amount),
            liquidation_fee: UsdcAtoms::new(1),
        }),
        (-600i32..600, -600i32..600, any::<bool>()).prop_map(|(from, to, zero_for_one)| {
            MarketEvent::TicksCrossed {
                starting_tick: from,
                ending_tick: to,
                zero_for_one,
            }
        }),
        (0u128..1_000_000, 0u128..1_000_000).prop_map(|(l, s)| MarketEvent::CapacityUpdated {
            capacity: per_side(l, s),
        }),
        (0u128..1_000_000, 0u128..1_000_000).prop_map(|(l, s)| MarketEvent::OpenInterestUpdated {
            open_interest: per_side(l, s),
        }),
        (1u64..1_000, 1u64..1_000, 0u64..100_000).prop_map(|(a, i, touch)| {
            MarketEvent::RatesAndEmasRefreshed {
                funding_per_day: FundingRate::from_wad(a as i128),
                util_fee_per_day: PerSide::new(
                    UtilizationRate::from_wad(a),
                    UtilizationRate::from_wad(i),
                ),
                last_touch: 1_700_000_000 + touch,
                pool_price_ema: price(a),
                index_ema: price(i),
            }
        }),
        (1u64..1_000).prop_map(|i| MarketEvent::IndexUpdated { index: price(i) }),
        (-1_000_000i128..1_000_000, 0u128..1_000_000_000).prop_map(|(d, m)| {
            MarketEvent::MarginTransferred {
                margin_delta: UsdcDelta::new(d),
                total_margin: UsdcAtoms::new(m),
            }
        }),
        (0u128..1_000_000).prop_map(|d| MarketEvent::BadDebtAccounted {
            bad_debt: UsdcAtoms::new(d),
            insurance_after: UsdcAtoms::ZERO,
            bad_debt_after: UsdcAtoms::new(d),
        }),
        (0u128..1_000_000).prop_map(|d| MarketEvent::LossSocialized {
            original_amount: UsdcAtoms::new(d),
            fee_charged: UsdcAtoms::ZERO,
            bad_debt_after: UsdcAtoms::new(d),
        }),
        (0u128..1_000_000).prop_map(|d| MarketEvent::Donated {
            donor: Address::repeat_byte(0xD0),
            amount: UsdcAtoms::new(1),
            bad_debt: UsdcAtoms::new(d),
            insurance: UsdcAtoms::ZERO,
        }),
        (0u8..6, 1u8..=255).prop_map(|(k, a)| MarketEvent::ModuleSet {
            module: [
                ModuleKind::Beacon,
                ModuleKind::Fees,
                ModuleKind::Funding,
                ModuleKind::MarginRatios,
                ModuleKind::PriceImpact,
                ModuleKind::Pricing,
            ][k as usize],
            address: Address::repeat_byte(a),
        }),
        (1u64..50, 0u8..=255, 0u8..=255).prop_map(|(pos, from, to)| {
            MarketEvent::PositionTransferred {
                from: Address::repeat_byte(from),
                to: Address::repeat_byte(to),
                pos_id: U256::from(pos),
            }
        }),
        (pos_id(), 0u128..1_000_000, 1u8..=255).prop_map(|(pos_id, margin, to)| {
            MarketEvent::MakerBackstopped {
                pos_id,
                margin_in: UsdcAtoms::new(margin),
                pos_recipient: Address::repeat_byte(to),
                settle: settle(),
            }
        }),
        (pos_id(), 0u128..1_000_000, 1u8..=255, -1_000i128..1_000).prop_map(
            |(pos_id, margin, to, funding)| MarketEvent::TakerBackstopped {
                pos_id,
                margin_in: UsdcAtoms::new(margin),
                pos_recipient: Address::repeat_byte(to),
                funding: UsdcDelta::new(funding),
                util_fees: UsdcAtoms::new(7),
            }
        ),
        (1u64..1_000_000).prop_map(|n| MarketEvent::CumulativesAccrued {
            cumulatives: cumulatives(n),
        }),
        (tick(), 1u64..1_000).prop_map(|(tick, n)| MarketEvent::TickInitialized {
            tick,
            cuml_funding_opp: Funding::from_x96(I256::try_from(n).unwrap()),
            cuml_funding_div_sqrt_p_opp: FundingPerSqrtPrice::from_x96(
                I256::try_from(n * 2).unwrap()
            ),
        }),
        tick().prop_map(|tick| MarketEvent::TickDeleted { tick }),
    ]
}

/// The variant's name, by a match with no wildcard: a new variant does not
/// compile until `event` produces it and this names it.
fn name_of(event: &MarketEvent) -> &'static str {
    match event {
        MarketEvent::MakerOpened { .. } => "MakerOpened",
        MarketEvent::MakerAdjusted { .. } => "MakerAdjusted",
        MarketEvent::MakerConverted { .. } => "MakerConverted",
        MarketEvent::MakerClosed { .. } => "MakerClosed",
        MarketEvent::MakerLiquidated { .. } => "MakerLiquidated",
        MarketEvent::MakerBackstopped { .. } => "MakerBackstopped",
        MarketEvent::TakerOpened { .. } => "TakerOpened",
        MarketEvent::TakerAdjusted { .. } => "TakerAdjusted",
        MarketEvent::TakerClosed { .. } => "TakerClosed",
        MarketEvent::TakerLiquidated { .. } => "TakerLiquidated",
        MarketEvent::TakerBackstopped { .. } => "TakerBackstopped",
        MarketEvent::CapacityUpdated { .. } => "CapacityUpdated",
        MarketEvent::OpenInterestUpdated { .. } => "OpenInterestUpdated",
        MarketEvent::CumulativesAccrued { .. } => "CumulativesAccrued",
        MarketEvent::RatesAndEmasRefreshed { .. } => "RatesAndEmasRefreshed",
        MarketEvent::TicksCrossed { .. } => "TicksCrossed",
        MarketEvent::TickInitialized { .. } => "TickInitialized",
        MarketEvent::TickDeleted { .. } => "TickDeleted",
        MarketEvent::Donated { .. } => "Donated",
        MarketEvent::BadDebtAccounted { .. } => "BadDebtAccounted",
        MarketEvent::LossSocialized { .. } => "LossSocialized",
        MarketEvent::MarginTransferred { .. } => "MarginTransferred",
        MarketEvent::IndexUpdated { .. } => "IndexUpdated",
        MarketEvent::PoolInitialized { .. } => "PoolInitialized",
        MarketEvent::PoolSwapped { .. } => "PoolSwapped",
        MarketEvent::ModifyLiquidity { .. } => "ModifyLiquidity",
        MarketEvent::PositionTransferred { .. } => "PositionTransferred",
        MarketEvent::ModuleSet { .. } => "ModuleSet",
    }
}

/// Every name `name_of` can return.
const VOCABULARY: [&str; 28] = [
    "MakerOpened",
    "MakerAdjusted",
    "MakerConverted",
    "MakerClosed",
    "MakerLiquidated",
    "MakerBackstopped",
    "TakerOpened",
    "TakerAdjusted",
    "TakerClosed",
    "TakerLiquidated",
    "TakerBackstopped",
    "CapacityUpdated",
    "OpenInterestUpdated",
    "CumulativesAccrued",
    "RatesAndEmasRefreshed",
    "TicksCrossed",
    "TickInitialized",
    "TickDeleted",
    "Donated",
    "BadDebtAccounted",
    "LossSocialized",
    "MarginTransferred",
    "IndexUpdated",
    "PoolInitialized",
    "PoolSwapped",
    "ModifyLiquidity",
    "PositionTransferred",
    "ModuleSet",
];

/// A few thousand draws produce every variant, so the law below is
/// checked over the whole vocabulary and not the part someone remembered.
#[test]
fn the_generator_speaks_the_whole_vocabulary() {
    let mut runner = TestRunner::deterministic();
    let seen: BTreeSet<&str> = (0..3_000)
        .map(|_| name_of(&event().new_tree(&mut runner).unwrap().current()))
        .collect();
    assert_eq!(seen, VOCABULARY.into_iter().collect());
}

/// A tape of up to a few hundred events in strictly increasing chain
/// order, a few per block, one transaction per block, so a cut inside a
/// block is a cut inside a transaction.
fn tape() -> impl Strategy<Value = Tape> {
    rows().prop_map(|rows| Tape::new(rows).expect("rows built in order are a tape"))
}

/// The rows of [`tape`], before the type sees them.
fn rows() -> impl Strategy<Value = Vec<TapeEvent>> {
    prop::collection::vec((event(), 1u64..4), 0..300).prop_map(|rows| {
        let mut block = 1;
        let mut index = 0;
        rows.into_iter()
            .map(|(event, step)| {
                // Advance a block every few rows; log index restarts in a new block.
                if step == 1 {
                    index += 1;
                } else {
                    block += step;
                    index = 0;
                }
                row(block, index, event)
            })
            .collect()
    })
}

proptest! {
    /// Rows in any order, some repeated, collect into the tape they came
    /// from; and a tape cut anywhere appends back into itself.
    #[test]
    fn a_tape_is_the_order_of_its_rows(
        (rows, shuffled) in rows().prop_flat_map(|rows| (Just(rows.clone()), Just(rows).prop_shuffle())),
        cut_at in 0.0f64..1.0,
    ) {
        let tape = Tape::new(rows).unwrap();
        let from_shuffled: Result<Tape, _> = shuffled.iter().chain(&shuffled).copied().collect();
        prop_assert_eq!(from_shuffled.unwrap(), tape.clone());

        let cut = ((tape.len() as f64) * cut_at) as usize;
        let (before, after) = tape.split_at(cut);
        let mut rejoined = before.to_owned();
        rejoined.append(after.to_owned()).unwrap();
        prop_assert_eq!(rejoined, tape);
    }

    /// `fold(a ++ b) == combine(fold(a), fold(b))` for every cut.
    #[test]
    fn the_fold_of_a_concatenation_is_the_combination_of_the_folds(
        tape in tape(),
        cut_at in 0.0f64..1.0,
    ) {
        let cut = ((tape.len() as f64) * cut_at) as usize;
        let whole = Replay::fold(&tape);
        let mut left = Replay::fold(&tape[..cut]);
        left.combine(Replay::fold(&tape[cut..]));
        prop_assert_eq!(left, whole);
    }

    /// Continuing a fold from a prefix equals folding the whole: a prefix
    /// is a checkpoint.
    #[test]
    fn a_prefix_is_a_checkpoint(tape in tape(), cut_at in 0.0f64..1.0) {
        let cut = ((tape.len() as f64) * cut_at) as usize;
        let whole = Replay::fold(&tape);
        let mut resumed = Replay::fold(&tape[..cut]);
        for row in &tape[cut..] {
            resumed.apply(row);
        }
        prop_assert_eq!(resumed, whole);
    }

    /// Combination is associative: three segments merge the same whichever
    /// pair merges first.
    #[test]
    fn combination_is_associative(tape in tape(), a in 0.0f64..1.0, b in 0.0f64..1.0) {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let i = ((tape.len() as f64) * lo) as usize;
        let j = ((tape.len() as f64) * hi) as usize;
        let (x, y, z) = (
            Replay::fold(&tape[..i]),
            Replay::fold(&tape[i..j]),
            Replay::fold(&tape[j..]),
        );
        let mut left_first = x.clone();
        left_first.combine(y.clone());
        left_first.combine(z.clone());
        let mut right_first = y;
        right_first.combine(z);
        let mut x = x;
        x.combine(right_first);
        prop_assert_eq!(left_first, x);
    }
}
