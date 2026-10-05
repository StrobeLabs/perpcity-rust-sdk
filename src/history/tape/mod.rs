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
mod view;

use std::borrow::Borrow;
use std::ops::Deref;
use std::result::Result as StdResult;
use std::slice;

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

/// A market's record: every event in chain order, no two at one point,
/// one hash and one timestamp per block, and timestamps that never
/// decrease. Every fold and every reading rests on that; the type owns
/// it, checked once when the rows arrive, so nothing downstream assumes
/// it.
///
/// Derefs to [`TapeSlice`], a run of rows that answers every question the
/// whole does, so a segment of a tape is a tape by type.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tape(Vec<TapeEvent>);

impl Tape {
    /// `rows`, if they are a tape.
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidTape`] at the first row that does not
    /// follow the one before it.
    pub fn new(rows: Vec<TapeEvent>) -> StdResult<Self, ValidationError> {
        for pair in rows.windows(2) {
            follows(&pair[0], &pair[1])?;
        }
        Ok(Self(rows))
    }

    /// The rows, in chain order.
    pub fn into_vec(self) -> Vec<TapeEvent> {
        self.0
    }
}

/// Whether `row` may follow `before` in a tape.
fn follows(before: &TapeEvent, row: &TapeEvent) -> StdResult<(), ValidationError> {
    let same_block = row.block_number == before.block_number;
    let reason = if row.point() <= before.point() {
        Some("not after the row before it")
    } else if same_block && row.block_hash != before.block_hash {
        Some("a second hash for its block")
    } else if same_block && row.timestamp != before.timestamp {
        Some("a second timestamp for its block")
    } else if row.timestamp < before.timestamp {
        Some("a timestamp before the block before it")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(ValidationError::InvalidTape {
            block: row.block_number,
            log_index: row.log_index,
            reason,
        }),
        None => Ok(()),
    }
}

impl Deref for Tape {
    type Target = TapeSlice;

    fn deref(&self) -> &TapeSlice {
        TapeSlice::from_rows(&self.0)
    }
}

impl AsRef<TapeSlice> for Tape {
    fn as_ref(&self) -> &TapeSlice {
        self
    }
}

impl AsRef<[TapeEvent]> for Tape {
    fn as_ref(&self) -> &[TapeEvent] {
        &self.0
    }
}

impl Borrow<TapeSlice> for Tape {
    fn borrow(&self) -> &TapeSlice {
        self
    }
}

impl IntoIterator for Tape {
    type Item = TapeEvent;
    type IntoIter = std::vec::IntoIter<TapeEvent>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a Tape {
    type Item = &'a TapeEvent;
    type IntoIter = slice::Iter<'a, TapeEvent>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// A run of a tape's rows, borrowed: in chain order, as the [`Tape`] they
/// were cut from. What a fold takes, and what a segment is.
#[derive(Debug, PartialEq)]
#[repr(transparent)]
pub struct TapeSlice([TapeEvent]);

impl TapeSlice {
    /// `rows` as a slice of a tape; private, since only rows cut from a
    /// tape are known to be one.
    fn from_rows(rows: &[TapeEvent]) -> &Self {
        // SAFETY: `TapeSlice` is `repr(transparent)` over `[TapeEvent]`,
        // so the two have one layout and one metadata; the cast changes
        // the type and nothing else, as `Path` is made from `OsStr`.
        unsafe { &*(rows as *const [TapeEvent] as *const TapeSlice) }
    }

    /// The rows, as a plain slice.
    pub fn as_rows(&self) -> &[TapeEvent] {
        &self.0
    }
}

impl Deref for TapeSlice {
    type Target = [TapeEvent];

    fn deref(&self) -> &[TapeEvent] {
        &self.0
    }
}

impl<'a> IntoIterator for &'a TapeSlice {
    type Item = &'a TapeEvent;
    type IntoIter = slice::Iter<'a, TapeEvent>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl ToOwned for TapeSlice {
    type Owned = Tape;

    fn to_owned(&self) -> Tape {
        Tape(self.0.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;

    use super::*;
    use crate::history::test_support::tape::{price, row};

    fn print(block: u64, index: u64) -> TapeEvent {
        row(block, index, MarketEvent::IndexUpdated { index: price(1) })
    }

    fn refused(rows: Vec<TapeEvent>) -> (u64, u64, &'static str) {
        match Tape::new(rows) {
            Err(ValidationError::InvalidTape {
                block,
                log_index,
                reason,
            }) => (block, log_index, reason),
            other => panic!("accepted or refused for another reason: {other:?}"),
        }
    }

    #[test]
    fn rows_in_chain_order_are_a_tape_and_read_as_their_rows() {
        let rows = vec![print(1, 0), print(1, 1), print(3, 0)];
        let tape = Tape::new(rows.clone()).unwrap();
        assert_eq!(tape.len(), 3);
        assert_eq!(tape[2].block_number, 3);
        assert_eq!(tape.iter().count(), 3);
        assert_eq!((&tape).into_iter().count(), 3);
        assert_eq!(tape.as_rows(), &rows[..]);
        assert_eq!(tape[..2].len(), 2, "a slice of the rows is a plain slice");
        assert_eq!(tape.clone().into_vec(), rows);
        assert!(Tape::new(Vec::new()).unwrap().is_empty());
    }

    #[test]
    fn a_row_that_does_not_follow_the_one_before_is_refused_by_name() {
        assert_eq!(
            refused(vec![print(2, 0), print(1, 0)]),
            (1, 0, "not after the row before it")
        );
        assert_eq!(
            refused(vec![print(1, 1), print(1, 1)]),
            (1, 1, "not after the row before it")
        );
        let mut other_hash = print(1, 1);
        other_hash.block_hash = B256::repeat_byte(0xEE);
        assert_eq!(
            refused(vec![print(1, 0), other_hash]),
            (1, 1, "a second hash for its block")
        );
        let mut other_time = print(1, 1);
        other_time.timestamp += 1;
        assert_eq!(
            refused(vec![print(1, 0), other_time]),
            (1, 1, "a second timestamp for its block")
        );
        let mut earlier = print(2, 0);
        earlier.timestamp = 0;
        assert_eq!(
            refused(vec![print(1, 0), earlier]),
            (2, 0, "a timestamp before the block before it")
        );
        let _ = U256::ZERO;
    }
}
