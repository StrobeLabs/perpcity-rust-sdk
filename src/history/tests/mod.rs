//! Tests for the history readers, over the in-memory
//! [`FakeNode`](super::test_support::FakeNode): one file per module under
//! test, the fixtures they share here.

mod beacon;
mod handle;
mod recording;
mod refusals;
mod scan;
mod tape;
mod transfers;

use std::fs;
use std::time::Duration;

use alloy::primitives::{
    Address, B256, Bytes, Log as PrimitiveLog, LogData, U256, address, b256, bytes, keccak256,
};
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::SolEvent;
use futures_util::TryStreamExt;

use super::scan::{SharedWidths, scan_newest};
use super::test_support::{
    CHAIN_ID, FakeNode, Mode, hash_of, mined_event_log, mined_log, timestamp_of,
};
use super::*;
use crate::constants::Q96;
use crate::contracts::{IBeacon, IERC20, IPoolManagerState, Perp, SwapResult};
use crate::convert::pack_balance_delta;
use crate::errors::{ContractError, PerpCityError, ValidationError};
use crate::events::MarketEvent;
use crate::units::Price;

const EMITTER: Address = Address::repeat_byte(0xAA);
const TOPIC: B256 = B256::repeat_byte(0x11);

/// One log every `step` blocks in `0..=last`.
fn logs_every(step: u64, last: u64) -> Vec<Log> {
    (0..=last)
        .step_by(step as usize)
        .map(|block| mined_log(EMITTER, TOPIC, Bytes::new(), block, 0))
        .collect()
}

fn filter() -> Filter {
    Filter::new().address(EMITTER).event_signature(TOPIC)
}

fn blocks(logs: &[Log]) -> Vec<u64> {
    logs.iter().map(|log| log.block_number.unwrap()).collect()
}

/// The accepted requests tile `from..=to` with no gap and no overlap.
fn assert_tiles(accepted: &[(u64, u64)], from: u64, to: u64) {
    let mut sorted = accepted.to_vec();
    sorted.sort_unstable();
    let mut next = from;
    for &(start, end) in &sorted {
        assert_eq!(start, next, "gap or overlap before {start} in {sorted:?}");
        next = end + 1;
    }
    assert_eq!(next, to + 1, "range not covered to its end: {sorted:?}");
}

const BEACON: Address = Address::repeat_byte(0xBE);

fn print_log(block: u64, index: u64, value: U256, timestamp: Option<u64>) -> Log {
    let mut log = mined_log(
        BEACON,
        IBeacon::IndexUpdated::SIGNATURE_HASH,
        value.to_be_bytes_vec().into(),
        block,
        index,
    );
    log.block_timestamp = timestamp;
    log
}

const USDC: Address = address!("af88d065e77c8cc2239327c5edb3a432268e5831");
const TREASURY: Address = Address::repeat_byte(0x01);
const WALLET_A: Address = Address::repeat_byte(0x0A);
const WALLET_B: Address = Address::repeat_byte(0x0B);
const OUTSIDER: Address = Address::repeat_byte(0x0C);

fn transfer_log(from: Address, to: Address, value: u64, block: u64) -> Log {
    let mut log = mined_log(USDC, B256::ZERO, Bytes::new(), block, 0);
    log.inner.data = LogData::new_unchecked(
        vec![
            IERC20::Transfer::SIGNATURE_HASH,
            from.into_word(),
            to.into_word(),
        ],
        U256::from(value).to_be_bytes_vec().into(),
    );
    log
}

const PERP: Address = Address::repeat_byte(0xF0);

fn taker_opened(pos_id: u64) -> Perp::TakerOpened {
    Perp::TakerOpened {
        posId: U256::from(pos_id),
        sr: SwapResult {
            delta: pack_balance_delta(100_000_000, -100_000_000),
            ammPrice: Q96,
            totalFeeAmt: alloy::primitives::I256::try_from(1_000_000i64).unwrap(),
            lpFeeAmt: U256::from(700_000u64),
            protocolFeeAmt: U256::from(100_000u64),
            creatorFeeAmt: U256::from(100_000u64),
            insuranceFeeAmt: U256::from(100_000u64),
        },
    }
}

fn position_mint(owner: Address, pos_id: u64) -> Perp::Transfer {
    position_transfer(Address::ZERO, owner, pos_id)
}

fn position_transfer(from: Address, to: Address, pos_id: u64) -> Perp::Transfer {
    Perp::Transfer {
        from,
        to,
        tokenId: U256::from(pos_id),
    }
}

const POOL_MANAGER: Address = Address::repeat_byte(0x9A);
const POOL_ID: B256 = B256::repeat_byte(0x77);

fn addresses() -> TapeAddresses {
    TapeAddresses {
        perp: PERP,
        beacon: BEACON,
        pool_manager: POOL_MANAGER,
        pool_id: POOL_ID,
    }
}

fn liquidity_change(pool_id: B256, pos_id: u64, delta: i128) -> IPoolManagerState::ModifyLiquidity {
    IPoolManagerState::ModifyLiquidity {
        id: pool_id,
        sender: PERP,
        tickLower: alloy::primitives::Signed::<24, 1>::try_from(-60).unwrap(),
        tickUpper: alloy::primitives::Signed::<24, 1>::try_from(60).unwrap(),
        liquidityDelta: alloy::primitives::I256::try_from(delta).unwrap(),
        salt: B256::from(U256::from(pos_id)),
    }
}
