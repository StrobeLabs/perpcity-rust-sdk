//! What a run of a tape answers about where its rows sit: the rows by
//! block, by transaction and by window of time; cuts between blocks for
//! a fold per core; and lookups by point, block and time, each a binary
//! search over rows in chain order.

use std::iter;
use std::ops::{Bound, RangeBounds};

use crate::history::fold::Window;

use super::{ChainPoint, TapeEvent, TapeSlice};

impl TapeSlice {
    /// The first and last points, if there are rows.
    pub fn span(&self) -> Option<(ChainPoint, ChainPoint)> {
        Some((self.first()?.point(), self.last()?.point()))
    }

    /// The rows block by block, in order.
    pub fn blocks(&self) -> impl Iterator<Item = &TapeSlice> {
        self.0
            .chunk_by(|a, b| a.block_number == b.block_number)
            .map(TapeSlice::from_rows)
    }

    /// The rows transaction by transaction, in order. A transaction's logs
    /// are contiguous within its block, so a run is one transaction.
    pub fn transactions(&self) -> impl Iterator<Item = &TapeSlice> {
        self.0
            .chunk_by(|a, b| a.block_number == b.block_number && a.tx_hash == b.tx_hash)
            .map(TapeSlice::from_rows)
    }

    /// The rows in consecutive windows of `window`, each opening at its
    /// first row's timestamp, so no window is empty and a quiet stretch
    /// is skipped rather than yielded.
    pub fn windows(&self, window: Window) -> impl Iterator<Item = &TapeSlice> {
        let secs = window.as_secs();
        let mut rest = &self.0;
        iter::from_fn(move || {
            let opens = rest.first()?.timestamp;
            let end = rest
                .partition_point(|row| row.timestamp < opens.saturating_add(secs))
                .max(1);
            let (window, after) = rest.split_at(end);
            rest = after;
            Some(TapeSlice::from_rows(window))
        })
    }

    /// The rows in up to `parts` runs of near-equal length, cut only
    /// between blocks: the segments a fold per core takes, each a tape.
    pub fn segments(&self, parts: usize) -> impl Iterator<Item = &TapeSlice> {
        let target = self.len().div_ceil(parts.max(1)).max(1);
        let mut rest = &self.0;
        iter::from_fn(move || {
            if rest.is_empty() {
                return None;
            }
            let mut end = target.min(rest.len());
            while end < rest.len() && rest[end].block_number == rest[end - 1].block_number {
                end += 1;
            }
            let (segment, after) = rest.split_at(end);
            rest = after;
            Some(TapeSlice::from_rows(segment))
        })
    }

    /// The row at `point`, if one is there.
    pub fn at(&self, point: ChainPoint) -> Option<&TapeEvent> {
        let index = self
            .0
            .binary_search_by(|row| row.point().cmp(&point))
            .ok()?;
        Some(&self.0[index])
    }

    /// The rows whose point is in `range`.
    pub fn between(&self, range: impl RangeBounds<ChainPoint>) -> &TapeSlice {
        self.within(range, TapeEvent::point)
    }

    /// The rows whose block is in `range`.
    pub fn in_blocks(&self, range: impl RangeBounds<u64>) -> &TapeSlice {
        self.within(range, |row| row.block_number)
    }

    /// The rows whose timestamp is in `range`.
    pub fn in_time(&self, range: impl RangeBounds<u64>) -> &TapeSlice {
        self.within(range, |row| row.timestamp)
    }

    /// The rows whose `key` is in `range`; `key` is monotone over a tape,
    /// so both ends are binary searches.
    fn within<K: Ord>(
        &self,
        range: impl RangeBounds<K>,
        key: impl Fn(&TapeEvent) -> K,
    ) -> &TapeSlice {
        let start = match range.start_bound() {
            Bound::Included(k) => self.0.partition_point(|row| key(row) < *k),
            Bound::Excluded(k) => self.0.partition_point(|row| key(row) <= *k),
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(k) => self.0.partition_point(|row| key(row) <= *k),
            Bound::Excluded(k) => self.0.partition_point(|row| key(row) < *k),
            Bound::Unbounded => self.0.len(),
        };
        TapeSlice::from_rows(&self.0[start..end.max(start)])
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::B256;

    use super::*;
    use crate::events::MarketEvent;
    use crate::history::tape::Tape;
    use crate::history::test_support::tape::{price, row};

    fn print(block: u64, index: u64) -> TapeEvent {
        row(block, index, MarketEvent::IndexUpdated { index: price(1) })
    }

    fn point(block: u64, log_index: u64) -> ChainPoint {
        ChainPoint { block, log_index }
    }

    /// Six rows over four blocks, ten seconds a block; block 3 holds two
    /// transactions.
    fn tape() -> Tape {
        let mut second_tx = print(3, 2);
        second_tx.tx_hash = B256::repeat_byte(0x33);
        Tape::new(vec![
            print(1, 0),
            print(1, 1),
            print(3, 0),
            print(3, 1),
            second_tx,
            print(7, 0),
        ])
        .unwrap()
    }

    fn lengths<'a>(runs: impl Iterator<Item = &'a TapeSlice>) -> Vec<usize> {
        runs.map(|run| run.len()).collect()
    }

    #[test]
    fn rows_group_by_block_by_transaction_and_by_window() {
        let tape = tape();
        assert_eq!(lengths(tape.blocks()), vec![2, 3, 1]);
        assert_eq!(lengths(tape.transactions()), vec![2, 2, 1, 1]);
        // Blocks are ten seconds apart: a 25-second window opened at block
        // 1 holds block 3 and not block 7; the next opens at block 7.
        assert_eq!(lengths(tape.windows(Window::seconds(25))), vec![5, 1]);
        assert_eq!(
            lengths(tape.windows(Window::seconds(0))),
            vec![1, 1, 1, 1, 1, 1],
            "a zero window still moves"
        );
        assert!(Tape::default().blocks().next().is_none());
    }

    #[test]
    fn segments_cut_only_between_blocks_and_cover_the_tape() {
        let tape = tape();
        let segments: Vec<&TapeSlice> = tape.segments(2).collect();
        assert_eq!(
            lengths(segments.iter().copied()),
            vec![5, 1],
            "block 3 is not split"
        );
        assert_eq!(lengths(tape.segments(1)), vec![6]);
        assert_eq!(
            lengths(tape.segments(100)),
            vec![2, 3, 1],
            "no finer than a block"
        );
        let rejoined: Vec<&TapeEvent> = tape.segments(3).flatten().collect();
        assert_eq!(rejoined.len(), 6);
        assert!(Tape::default().segments(4).next().is_none());
    }

    #[test]
    fn lookups_by_point_block_and_time_are_the_sorted_maps() {
        let tape = tape();
        assert_eq!(tape.at(point(3, 1)).map(|r| r.log_index), Some(1));
        assert_eq!(tape.at(point(2, 0)), None);
        assert_eq!(tape.between(point(1, 1)..point(3, 1)).len(), 2);
        assert_eq!(tape.between(point(1, 1)..=point(3, 1)).len(), 3);
        assert_eq!(tape.between(..point(3, 0)).len(), 2);
        assert_eq!(tape.between(point(7, 0)..).len(), 1);
        assert_eq!(tape.in_blocks(2..=3).len(), 3);
        assert_eq!(tape.in_blocks(4..6).len(), 0);
        let t = |block: u64| 1_700_000_000 + block * 10;
        assert_eq!(tape.in_time(t(1)..t(7)).len(), 5);
        assert_eq!(tape.in_time(..=t(1)).len(), 2);
        assert_eq!(tape.span(), Some((point(1, 0), point(7, 0))));
        assert_eq!(Tape::default().span(), None);
    }
}
