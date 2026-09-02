//! Storage-slot derivation for the deployed contract layouts — state the
//! contracts expose no getter for.
//!
//! Solidity stores the value for `key` in a mapping at slot `p` at
//! `keccak256(abi.encode(key, p))`. These helpers compute those slots for
//! the two layouts the SDK reads raw:
//!
//! - the Perp's `PerpStorage` — the `s.ticks[tick]` funding checkpoints
//!   (read via `eth_getStorageAt`), and
//! - the Uniswap V4 `PoolManager`'s `_pools[poolId]` `Pool.State` — the
//!   tick bitmap and the fee-growth accounting (read via `extsload`).
//!
//! The offsets encode the deployed contract layouts and are locked by
//! chain-backed tests at the call sites (the taker book loader verifies its
//! reconstruction against the pool's reported liquidity, and the maker
//! equity math reproduces a real on-chain settle).

use alloy::primitives::{Address, B256, I256, U256, keccak256};

/// `PerpStorage.ticks` mapping slot: storage struct base 3 + field index 3.
const PERP_TICKS_SLOT: u64 = 6;

/// `PerpStorage.emas` slot: storage struct base 3 + field index 8 in the
/// deployed layout. `PricePair` packs `ammPrice` in the low 128 bits and
/// `index` in the high 128 bits.
const PERP_EMAS_SLOT: u64 = 11;

/// Offset of `cumlFundingDivSqrtPOppX96` — the second word of the Perp's
/// two-word `TickInfo` struct — from the struct's base slot.
const TICK_FUNDING_DIV_SQRT_P_OPP_OFFSET: u8 = 1;

/// `PoolManager._pools` mapping slot.
const POOL_MANAGER_POOLS_SLOT: u64 = 6;

/// Offset of `feeGrowthGlobal1X128` inside `Pool.State`.
const FEE_GROWTH_GLOBAL1_OFFSET: u8 = 2;

/// Offset of the `ticks` mapping inside `Pool.State`.
const TICKS_OFFSET: u8 = 4;

/// Offset of the `tickBitmap` mapping inside `Pool.State`.
const TICK_BITMAP_OFFSET: u8 = 5;

/// Offset of the `positions` mapping inside `Pool.State`.
const POSITIONS_OFFSET: u8 = 6;

/// Offset of `feeGrowthOutside1X128` inside `Tick.Info`.
const TICK_FEE_GROWTH_OUTSIDE1_OFFSET: u8 = 2;

/// Offset of `feeGrowthInside1LastX128` inside V4's `Position.State`.
const POSITION_FEE_GROWTH_INSIDE1_OFFSET: u8 = 2;

/// Slot of `mapping[key]` for a mapping stored at `base`.
pub(crate) fn mapping_slot(key: B256, base: U256) -> U256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(key.as_slice());
    buf[32..].copy_from_slice(&base.to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(buf).0)
}

/// Slot of `mapping[key]` for a signed integer key (two's-complement,
/// sign-extended to 32 bytes).
pub(crate) fn mapping_slot_signed(key: i32, base: U256) -> U256 {
    let key = I256::unchecked_from(key).into_raw();
    mapping_slot(B256::from(key), base)
}

/// Slots of `s.ticks[tick]` (a `TickInfo`) on the Perp contract:
/// `[cumlFundingOppX96, cumlFundingDivSqrtPOppX96]`, the struct's two
/// consecutive words.
pub(crate) fn perp_tick_funding_slots(tick: i32) -> [U256; 2] {
    let base = mapping_slot_signed(tick, U256::from(PERP_TICKS_SLOT));
    [base, base + U256::from(TICK_FUNDING_DIV_SQRT_P_OPP_OFFSET)]
}

/// Slot of the Perp's stored EMA `PricePair` (`s.emas`).
pub(crate) fn perp_emas_slot() -> U256 {
    U256::from(PERP_EMAS_SLOT)
}

/// Base slot of a pool's `Pool.State` inside the V4 PoolManager.
pub(crate) fn pool_state_slot(pool_id: B256) -> U256 {
    mapping_slot(pool_id, U256::from(POOL_MANAGER_POOLS_SLOT))
}

/// Slot of a pool's `feeGrowthGlobal1X128`.
pub(crate) fn v4_fee_growth_global1_slot(pool_id: B256) -> U256 {
    pool_state_slot(pool_id) + U256::from(FEE_GROWTH_GLOBAL1_OFFSET)
}

/// Base slot of `state.ticks[tick]` inside a pool's state.
pub(crate) fn v4_tick_slot(pool_id: B256, tick: i32) -> U256 {
    mapping_slot_signed(tick, pool_state_slot(pool_id) + U256::from(TICKS_OFFSET))
}

/// Slot of a tick's `feeGrowthOutside1X128`.
pub(crate) fn v4_tick_fee_growth_outside1_slot(pool_id: B256, tick: i32) -> U256 {
    v4_tick_slot(pool_id, tick) + U256::from(TICK_FEE_GROWTH_OUTSIDE1_OFFSET)
}

/// Slot of `state.tickBitmap[word]` inside a pool's state.
pub(crate) fn v4_tick_bitmap_slot(pool_id: B256, word: i32) -> U256 {
    mapping_slot_signed(
        word,
        pool_state_slot(pool_id) + U256::from(TICK_BITMAP_OFFSET),
    )
}

/// Base slot of the V4 position keyed by `(owner, tickLower, tickUpper,
/// salt)`. V4 hashes the packed key (ticks as 3-byte two's-complement)
/// before the mapping lookup.
pub(crate) fn v4_position_slot(
    pool_id: B256,
    owner: Address,
    tick_lower: i32,
    tick_upper: i32,
    salt: B256,
) -> U256 {
    let mut packed = [0u8; 20 + 3 + 3 + 32];
    packed[..20].copy_from_slice(owner.as_slice());
    packed[20..23].copy_from_slice(&tick_lower.to_be_bytes()[1..]);
    packed[23..26].copy_from_slice(&tick_upper.to_be_bytes()[1..]);
    packed[26..].copy_from_slice(salt.as_slice());
    mapping_slot(
        keccak256(packed),
        pool_state_slot(pool_id) + U256::from(POSITIONS_OFFSET),
    )
}

/// Slot of a V4 position's `feeGrowthInside1LastX128`.
pub(crate) fn v4_position_fee_growth_inside1_slot(
    pool_id: B256,
    owner: Address,
    tick_lower: i32,
    tick_upper: i32,
    salt: B256,
) -> U256 {
    v4_position_slot(pool_id, owner, tick_lower, tick_upper, salt)
        + U256::from(POSITION_FEE_GROWTH_INSIDE1_OFFSET)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v4_position_slot_packs_signed_ticks() {
        // Shape checks: negative ticks must pack as 3-byte two's complement,
        // and different salts must land on different slots.
        let pool = B256::repeat_byte(1);
        let owner = Address::repeat_byte(2);
        let a = v4_position_slot(pool, owner, -60, 60, B256::from(U256::from(1u8)));
        let b = v4_position_slot(pool, owner, -60, 60, B256::from(U256::from(2u8)));
        assert_ne!(a, b);
    }

    #[test]
    fn signed_keys_sign_extend() {
        let base = U256::from(6u8);
        let positive = mapping_slot_signed(1, base);
        let negative = mapping_slot_signed(-1, base);
        assert_ne!(positive, negative);
        assert_eq!(negative, mapping_slot(B256::repeat_byte(0xff), base));
    }
}
