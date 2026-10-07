//! Rows for a fold's tests: the builders a fixture tape is written with,
//! and the law every fold is held to.
//!
//! The builders fix what a test does not care about, the fees of a swap
//! or the shape of a settlement, so a fixture reads as the events it is
//! about. Public under the `test-utils` feature, so a fold in a crate
//! built on the replay is tested over the same rows and held to the same
//! law.

use std::fmt::Debug;

use alloy::primitives::{Address, B256, U256};

use crate::constants::Q96;
use crate::events::{MakerSettle, MarketEvent, SwapInfo};
use crate::history::fold::Fold;
use crate::history::tape::TapeEvent;
use crate::math::tick::get_sqrt_ratio_at_tick;
use crate::units::{LDelta, LUnits, PerSide, PerpAtoms, PerpDelta, Price, UsdcAtoms, UsdcDelta};

use super::hash_of;

/// A row in `block` at `log_index`, ten seconds a block, with one
/// transaction per block, so a fixture's transaction is its block's and
/// no two blocks share one.
pub fn row(block: u64, log_index: u64, event: MarketEvent) -> TapeEvent {
    let mut tx_hash = B256::from(U256::from(block));
    tx_hash.0[0] = 0x7A;
    TapeEvent {
        block_number: block,
        block_hash: hash_of(block),
        log_index,
        timestamp: 1_700_000_000 + block * 10,
        tx_hash,
        event,
    }
}

/// `units` as a price, in whole units.
pub fn price(units: u64) -> Price {
    Price::from_x96(Q96 * U256::from(units))
}

/// A long and a short, in perp atoms.
pub fn per_side(long: u128, short: u128) -> PerSide<PerpAtoms> {
    PerSide::new(PerpAtoms::new(long), PerpAtoms::new(short))
}

/// A swap of `perp_delta` at `pool_price` for 40 USDC, with fixed fees:
/// 70,000 to the pool, 10,000 each to the protocol and the creator, and
/// `insurance_fee` to insurance, under a stated total of 100,000.
pub fn swap(perp_delta: i128, pool_price: Price, insurance_fee: u128) -> SwapInfo {
    SwapInfo {
        perp_delta: PerpDelta::new(perp_delta),
        usd_delta: UsdcDelta::new(-40_000_000),
        pool_price,
        total_fee: UsdcDelta::new(100_000),
        lp_fee: UsdcAtoms::new(70_000),
        protocol_fee: UsdcAtoms::new(10_000),
        creator_fee: UsdcAtoms::new(10_000),
        insurance_fee: UsdcAtoms::new(insurance_fee),
    }
}

/// A maker's settlement: 1,000 of funding, utilization fees of 10 and 20,
/// 30 in pool fees.
pub fn settle() -> MakerSettle {
    MakerSettle {
        funding: UsdcDelta::new(1_000),
        util_fees: PerSide::new(UsdcAtoms::new(10), UsdcAtoms::new(20)),
        lp_fees: UsdcAtoms::new(30),
    }
}

/// A liquidity change of `delta` for position `pos` over `lower..upper`,
/// salted with the position id as the live build does.
pub fn modify(pos: u64, lower: i32, upper: i32, delta: i128) -> MarketEvent {
    MarketEvent::ModifyLiquidity {
        pool_id: B256::with_last_byte(7),
        sender: Address::ZERO,
        tick_lower: lower,
        tick_upper: upper,
        liquidity_delta: LDelta::new(delta),
        salt: B256::from(U256::from(pos)),
    }
}

/// The pool created at `tick`'s exact price.
///
/// # Panics
///
/// For a tick outside the pool's domain.
pub fn pool_initialized(tick: i32) -> MarketEvent {
    MarketEvent::PoolInitialized {
        pool_id: B256::with_last_byte(7),
        sqrt_price: get_sqrt_ratio_at_tick(tick).expect("a tick in the pool's domain"),
        tick,
    }
}

/// A swap that left the pool at `tick`'s exact price.
///
/// # Panics
///
/// For a tick outside the pool's domain.
pub fn pool_swapped(tick: i32) -> MarketEvent {
    MarketEvent::PoolSwapped {
        pool_id: B256::with_last_byte(7),
        sender: Address::ZERO,
        sqrt_price: get_sqrt_ratio_at_tick(tick).expect("a tick in the pool's domain"),
        liquidity: LUnits::ZERO,
        tick,
    }
}

/// The law every fold is held to: `fold(a ++ b) == combine(fold(a), fold(b))`
/// at every cut of `tape`, between blocks and inside them, from `start`.
/// `start` is `F::default()` for a segment, or the fold's own genesis.
///
/// # Panics
///
/// At the first cut where the combination differs from the whole.
pub fn assert_combine_law<F>(start: F, tape: &[TapeEvent])
where
    F: Fold + Clone + PartialEq + Debug,
{
    let mut whole = start.clone();
    for row in tape {
        whole.apply(row);
    }
    for cut in 0..=tape.len() {
        let mut left = start.clone();
        for row in &tape[..cut] {
            left.apply(row);
        }
        left.combine(F::fold(&tape[cut..]));
        assert_eq!(left, whole, "cut at {cut}");
    }
}
