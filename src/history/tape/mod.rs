//! The market-event tape: a perp's event history, replayed through the
//! same decoder the live feed uses.
//!
//! A [`TapeEvent`] is a [`MarketEvent`] with its chain position, so a
//! stored tape row and a live feed event carry the same vocabulary — the
//! only difference is which transport delivered the log. Logs the decoder
//! does not recognize (ERC-721 approvals, admin events) are skipped, as
//! the live feed skips them.
//!
//! [`market_events`] covers what the perp itself emits. That includes
//! [`MarketEvent::PositionTransferred`] — the position NFT's mint, burn
//! and mid-life transfers — so who held a position at a given block is a
//! fold of the tape: [`OwnershipLog`]. Trade events name a position id
//! and never a wallet, so that fold is how a market's activity is
//! attributed to the addresses behind it. A market's state has two more
//! sources, at other addresses: its index, [`MarketEvent::IndexUpdated`]
//! from the beacon, and its liquidity, [`MarketEvent::ModifyLiquidity`] from
//! the PoolManager, which logs every perp pool's liquidity change under
//! the pool's id. [`market_tape`] reads all three in one chain order, so a
//! fold that rebuilds the market from its events has everything the chain
//! said about it; [`market_events`] stays the one-address scan.
//!
//! Chain order has two sources: within one response, every production
//! client returns `eth_getLogs` results by block then log index; across
//! responses, the scan reads its windows in range order. Folds that
//! depend on it say so — [`OwnershipLog::fold`] debug-asserts each
//! position's transfers arrive strictly increasing.

mod custody;
mod read;

use std::result::Result as StdResult;

use alloy::primitives::{Address, B256};
use alloy::rpc::types::Log;
use serde::{Deserialize, Serialize};

use crate::errors::ValidationError;
use crate::events::MarketEvent;

use super::fold::Sample;

pub use self::custody::OwnershipLog;
pub use self::read::{latest_market_events, market_events, market_tape};
pub(in crate::history) use self::read::{
    latest_market_events_with, market_events_with, market_logs_with, market_tape_with,
    stamp_timestamps,
};

/// A position in the chain's total order: a block, then a log's index
/// within it.
///
/// Two events in the same block still compare, which is what a join
/// across series (a fill against the index print before it, a trade
/// against the transfer that handed the position over) needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChainPoint {
    /// Block the log landed in.
    pub block: u64,
    /// Position of the log within its block.
    pub log_index: u64,
}

/// One market event with its chain position.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TapeEvent {
    /// Block the event landed in.
    pub block_number: u64,
    /// Hash of that block, so a state rebuilt from the tape carries the
    /// same block identity a pinned read does.
    pub block_hash: B256,
    /// Position of the event's log in its block.
    pub log_index: u64,
    /// Unix timestamp of the block.
    pub timestamp: u64,
    /// Transaction that emitted the event.
    pub tx_hash: B256,
    /// The decoded event, as the live feed would have streamed it.
    pub event: MarketEvent,
}

impl TapeEvent {
    /// Where this event sits in the chain's total order.
    pub fn point(&self) -> ChainPoint {
        ChainPoint {
            block: self.block_number,
            log_index: self.log_index,
        }
    }

    /// `value` as a sample at this event's point and time.
    pub fn sample<T>(&self, value: T) -> Sample<T> {
        Sample {
            point: self.point(),
            timestamp: self.timestamp,
            value,
        }
    }

    /// `event`, decoded from `log`, stamped with the log's mined position
    /// and `timestamp`: the one constructor both tenses use, so a feed's
    /// event and a scan's are the same row.
    ///
    /// # Errors
    ///
    /// [`ValidationError::DecodeFailed`] for a log without a mined
    /// position — a pending log, which neither tense should hand here.
    pub(crate) fn stamped(
        log: &Log,
        event: MarketEvent,
        timestamp: u64,
    ) -> StdResult<Self, ValidationError> {
        let position = log
            .block_number
            .zip(log.block_hash)
            .zip(log.log_index)
            .zip(log.transaction_hash);
        let (((block_number, block_hash), log_index), tx_hash) =
            position.ok_or_else(|| ValidationError::DecodeFailed {
                context: format!(
                    "market event log from {} in tx {:?} has no mined position",
                    log.address(),
                    log.transaction_hash
                ),
            })?;
        Ok(Self {
            block_number,
            block_hash,
            log_index,
            timestamp,
            tx_hash,
            event,
        })
    }
}

/// The three addresses a market's tape is read from.
///
/// A market's events come from its own contract; its index from the
/// beacon it is configured with; its liquidity from the shared PoolManager,
/// which logs every pool's liquidity under the pool's id. The client knows
/// all three ([`MarketReader::tape_addresses`](crate::MarketReader::tape_addresses));
/// the handle only needs to be told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TapeAddresses {
    /// The market.
    pub perp: Address,
    /// The beacon the market reads its index from.
    pub beacon: Address,
    /// The chain's PoolManager.
    pub pool_manager: Address,
    /// The market's pool, as the PoolManager keys it.
    pub pool_id: B256,
}
