//! The law a replay rests on, checked across random tapes rather than one:
//! the fold of a concatenation is the combination of the folds, at any cut,
//! so a fold of any prefix is a checkpoint and segments fold on separate
//! cores.

use alloy::primitives::{Address, B256, U256};
use perpcity_sdk::events::{MarketEvent, ModuleKind};
use perpcity_sdk::history::test_support::tape::{per_side, price, settle, swap};
use perpcity_sdk::history::{Fold, Replay, TapeEvent};
use perpcity_sdk::{
    FundingRate, LDelta, LUnits, PerSide, PerpAtoms, UsdcAtoms, UsdcDelta, UtilizationRate,
};
use proptest::prelude::*;

/// A position id from a small pool, so each position sees many events.
fn pos_id() -> impl Strategy<Value = U256> {
    (1u64..50).prop_map(U256::from)
}

/// One of the events the fold reads, with the values a market could emit:
/// the market's totals, the positions' lives, and the pool's liquidity.
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
        (0u128..1_000_000_000).prop_map(|m| MarketEvent::MarginTransferred {
            margin_delta: UsdcDelta::new(0),
            total_margin: UsdcAtoms::new(m),
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
    ]
}

/// A tape of up to a few hundred events in strictly increasing chain
/// order, a few per block.
fn tape() -> impl Strategy<Value = Vec<TapeEvent>> {
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
                TapeEvent {
                    block_number: block,
                    block_hash: B256::with_last_byte((block % 251) as u8),
                    log_index: index,
                    timestamp: 1_700_000_000 + block * 10,
                    tx_hash: B256::with_last_byte(3),
                    event,
                }
            })
            .collect()
    })
}

proptest! {
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
