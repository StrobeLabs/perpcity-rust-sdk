//! The two shapes a market's history takes, and the reading taken from
//! either.
//!
//! A [`Series`] is a value that holds between updates: the index, the pool
//! price, a stated total. It answers what the value was at a point, how it
//! changed over a window, where its peak was. [`Arrivals`] is a point
//! process: liquidations, swaps, prints, each with a mark. It answers how
//! many arrived in a window and where they were densest. Conflating the two
//! would make a nonsense query representable, which is why they are two
//! types.
//!
//! Both are kept in chain order and refuse a push at or before their last
//! point, as [`Sequenced`](super::Sequenced) does. Both append when
//! segments of a tape are combined, so a fold that holds them keeps the law
//! `fold(a ++ b) == combine(fold(a), fold(b))`. Both trim to a
//! [`Retention`], so a monitor keeps hours and a forensic fold keeps a life.
//!
//! A [`Reading`] is what a question returns: the value in its units, the
//! point it holds at, and the span of samples it was computed from, so an
//! alarm can name what moved it and a forensic bin can reproduce it from
//! the tape.

use std::time::Duration;

use alloy::primitives::B256;

use super::super::tape::ChainPoint;

/// A length of time a question is asked over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Window(Duration);

impl Window {
    /// `seconds` long.
    pub const fn seconds(seconds: u64) -> Self {
        Self(Duration::from_secs(seconds))
    }

    /// `minutes` long.
    pub const fn minutes(minutes: u64) -> Self {
        Self::seconds(minutes * 60)
    }

    /// `hours` long.
    pub const fn hours(hours: u64) -> Self {
        Self::minutes(hours * 60)
    }

    /// `days` long.
    pub const fn days(days: u64) -> Self {
        Self::hours(days * 24)
    }

    /// The window in seconds, the unit block timestamps carry.
    pub const fn as_secs(self) -> u64 {
        self.0.as_secs()
    }
}

/// How much history a series keeps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Retention {
    /// Everything since the fold's start: the forensic and research setting.
    #[default]
    All,
    /// The last window, by block timestamp, plus the one sample before it so
    /// the window's start can be answered: the monitor's setting.
    Last(Window),
}

/// A value at a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample<T> {
    /// Where the value was stated.
    pub point: ChainPoint,
    /// The block timestamp there.
    pub timestamp: u64,
    /// The value stated.
    pub value: T,
}

impl<T> Sample<T> {
    /// Before any event: the point a fold from genesis states its zeros at.
    pub const fn origin(value: T) -> Self {
        Self {
            point: ChainPoint {
                block: 0,
                log_index: 0,
            },
            timestamp: 0,
            value,
        }
    }

    /// The same point and time, carrying `value` instead.
    pub fn with<U>(&self, value: U) -> Sample<U> {
        Sample {
            point: self.point,
            timestamp: self.timestamp,
            value,
        }
    }
}

/// The samples a reading was computed from, first to last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// The first sample's point.
    pub from: ChainPoint,
    /// The last sample's point.
    pub to: ChainPoint,
}

/// A value read from a series or a point process, with its provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading<T> {
    /// The value, in its units.
    pub value: T,
    /// Where it holds: the point of the last sample it was computed from.
    pub point: ChainPoint,
    /// The block timestamp there.
    pub timestamp: u64,
    /// The samples it was computed from.
    pub moved_by: Span,
}

impl<T> Reading<T> {
    fn of(value: T, first: ChainPoint, last: &Sample<impl Copy>) -> Self {
        Self {
            value,
            point: last.point,
            timestamp: last.timestamp,
            moved_by: Span {
                from: first,
                to: last.point,
            },
        }
    }
}

/// A value then and now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change<T> {
    /// The value at the start of the window.
    pub from: T,
    /// The value now.
    pub to: T,
}

/// A value that holds between updates, in chain order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Series<T> {
    samples: Vec<Sample<T>>,
    retention: Retention,
    /// Whether retention has dropped samples, so an answer about a time
    /// before the earliest sample is `None` rather than the earliest.
    trimmed: bool,
}

impl<T> Default for Series<T> {
    fn default() -> Self {
        Self {
            samples: Vec::new(),
            retention: Retention::All,
            trimmed: false,
        }
    }
}

impl<T: Copy> Series<T> {
    /// A series holding one statement, as a read supplies it.
    pub fn stated(sample: Sample<T>) -> Self {
        Self {
            samples: vec![sample],
            ..Self::default()
        }
    }

    /// Keep only `retention` from here on, trimming now.
    pub fn retain(&mut self, retention: Retention) {
        self.retention = retention;
        self.trim();
    }

    /// Record `sample` if it is after the last; whether it was taken.
    pub fn push(&mut self, sample: Sample<T>) -> bool {
        if self
            .samples
            .last()
            .is_some_and(|last| sample.point <= last.point)
        {
            return false;
        }
        self.samples.push(sample);
        self.trim();
        true
    }

    /// The value as last stated, with no provenance: for a fold's own use.
    pub fn value(&self) -> Option<T> {
        self.samples.last().map(|s| s.value)
    }

    /// The value as last stated.
    pub fn latest(&self) -> Option<Reading<T>> {
        let last = self.samples.last()?;
        Some(Reading::of(last.value, last.point, last))
    }

    /// The value standing at `point`: the last sample at or before it.
    pub fn at(&self, point: ChainPoint) -> Option<Reading<T>> {
        let index = self.samples.partition_point(|s| s.point <= point);
        let sample = self.samples.get(index.checked_sub(1)?)?;
        Some(Reading::of(sample.value, sample.point, sample))
    }

    /// The value standing at block time `timestamp`: the last sample at or
    /// before it. `None` before the first sample, or before the earliest one
    /// kept, since a trimmed series does not know.
    pub fn at_time(&self, timestamp: u64) -> Option<Reading<T>> {
        let index = self.samples.partition_point(|s| s.timestamp <= timestamp);
        let sample = self.samples.get(index.checked_sub(1)?)?;
        Some(Reading::of(sample.value, sample.point, sample))
    }

    /// Every sample kept, in chain order.
    pub fn samples(&self) -> &[Sample<T>] {
        &self.samples
    }

    /// The samples inside the window ending at the latest one.
    pub fn window(&self, window: Window) -> &[Sample<T>] {
        let Some(last) = self.samples.last() else {
            return &[];
        };
        let from = last.timestamp.saturating_sub(window.as_secs());
        let start = self.samples.partition_point(|s| s.timestamp < from);
        &self.samples[start..]
    }

    /// The samples since block time `timestamp`, or `None` if retention has
    /// dropped some of them.
    pub fn since(&self, timestamp: u64) -> Option<&[Sample<T>]> {
        let start = self.samples.partition_point(|s| s.timestamp < timestamp);
        if start == 0 && self.trimmed {
            return None;
        }
        Some(&self.samples[start..])
    }

    /// The value at the start of the window and the value now.
    pub fn change_over(&self, window: Window) -> Option<Reading<Change<T>>> {
        let last = self.samples.last()?;
        let then = self.at_time(last.timestamp.saturating_sub(window.as_secs()))?;
        Some(Reading::of(
            Change {
                from: then.value,
                to: last.value,
            },
            then.point,
            last,
        ))
    }

    /// Append the segment after this one.
    pub fn combine(&mut self, later: Self) {
        for sample in later.samples {
            self.push(sample);
        }
        self.trimmed |= later.trimmed;
    }

    /// Drop what retention does not keep: everything before the window,
    /// except the last sample before it.
    fn trim(&mut self) {
        let Retention::Last(window) = self.retention else {
            return;
        };
        let Some(last) = self.samples.last() else {
            return;
        };
        let from = last.timestamp.saturating_sub(window.as_secs());
        let first_inside = self.samples.partition_point(|s| s.timestamp < from);
        let keep_from = first_inside.saturating_sub(1);
        if keep_from > 0 {
            self.samples.drain(..keep_from);
            self.trimmed = true;
        }
    }
}

impl<T: Copy + PartialOrd> Series<T> {
    /// The largest value in the window ending at the latest sample.
    pub fn peak(&self, window: Window) -> Option<Reading<T>> {
        let samples = self.window(window);
        let first = samples.first()?.point;
        let peak = samples
            .iter()
            .fold(None::<&Sample<T>>, |best, s| match best {
                Some(b) if b.value >= s.value => Some(b),
                _ => Some(s),
            })?;
        Some(Reading::of(peak.value, first, peak))
    }
}

/// One arrival of a point process, with its mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arrival<M> {
    /// Where it arrived.
    pub point: ChainPoint,
    /// The block timestamp there.
    pub timestamp: u64,
    /// The transaction it arrived in.
    pub tx: B256,
    /// What arrived.
    pub mark: M,
}

/// A point process in chain order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arrivals<M> {
    arrivals: Vec<Arrival<M>>,
    retention: Retention,
}

impl<M> Default for Arrivals<M> {
    fn default() -> Self {
        Self {
            arrivals: Vec::new(),
            retention: Retention::All,
        }
    }
}

impl<M> Arrivals<M> {
    /// Keep only `retention` from here on, trimming now.
    pub fn retain(&mut self, retention: Retention) {
        self.retention = retention;
        self.trim();
    }

    /// Record `arrival` if it is after the last; whether it was taken.
    pub fn push(&mut self, arrival: Arrival<M>) -> bool {
        if self
            .arrivals
            .last()
            .is_some_and(|last| arrival.point <= last.point)
        {
            return false;
        }
        self.arrivals.push(arrival);
        self.trim();
        true
    }

    /// Every arrival kept, in chain order.
    pub fn iter(&self) -> impl Iterator<Item = &Arrival<M>> {
        self.arrivals.iter()
    }

    /// Every arrival kept, as a slice.
    pub fn as_slice(&self) -> &[Arrival<M>] {
        &self.arrivals
    }

    /// Every arrival kept, mutably: for a fold filling a mark at a cut.
    /// Points are not to be changed.
    pub(crate) fn arrivals_mut(&mut self) -> &mut [Arrival<M>] {
        &mut self.arrivals
    }

    /// Drop the arrivals `unwanted` says so of: for a fold removing, at a
    /// cut, what the segment before it had already recorded.
    pub(crate) fn drop_where(&mut self, mut unwanted: impl FnMut(&Arrival<M>) -> bool) {
        self.arrivals.retain(|a| !unwanted(a));
    }

    /// The last arrival.
    pub fn last(&self) -> Option<&Arrival<M>> {
        self.arrivals.last()
    }

    /// Arrivals kept.
    pub fn len(&self) -> usize {
        self.arrivals.len()
    }

    /// Whether none has arrived.
    pub fn is_empty(&self) -> bool {
        self.arrivals.is_empty()
    }

    /// The arrivals inside the window ending at the latest one.
    pub fn window(&self, window: Window) -> &[Arrival<M>] {
        let Some(last) = self.arrivals.last() else {
            return &[];
        };
        let from = last.timestamp.saturating_sub(window.as_secs());
        let start = self.arrivals.partition_point(|a| a.timestamp < from);
        &self.arrivals[start..]
    }

    /// How many arrived in the window ending at the latest one; `None`
    /// before any has.
    pub fn count_in(&self, window: Window) -> Option<Reading<u32>> {
        let inside = self.window(window);
        let last = inside.last()?;
        Some(Reading {
            value: inside.len() as u32,
            point: last.point,
            timestamp: last.timestamp,
            moved_by: Span {
                from: inside[0].point,
                to: last.point,
            },
        })
    }

    /// The densest window of the given length: the most arrivals any such
    /// window holds, as the arrivals in it.
    pub fn busiest(&self, window: Window) -> Option<&[Arrival<M>]> {
        let secs = window.as_secs();
        let mut best: Option<(usize, usize)> = None;
        let mut start = 0;
        for end in 0..self.arrivals.len() {
            while self.arrivals[end].timestamp - self.arrivals[start].timestamp > secs {
                start += 1;
            }
            if best.is_none_or(|(b_start, b_end)| end + 1 - start > b_end - b_start) {
                best = Some((start, end + 1));
            }
        }
        best.map(|(start, end)| &self.arrivals[start..end])
    }

    /// Append the segment after this one.
    pub fn combine(&mut self, later: Self) {
        for arrival in later.arrivals {
            self.push(arrival);
        }
    }

    /// Drop what retention does not keep.
    fn trim(&mut self) {
        let Retention::Last(window) = self.retention else {
            return;
        };
        let Some(last) = self.arrivals.last() else {
            return;
        };
        let from = last.timestamp.saturating_sub(window.as_secs());
        let keep_from = self.arrivals.partition_point(|a| a.timestamp < from);
        if keep_from > 0 {
            self.arrivals.drain(..keep_from);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(block: u64, timestamp: u64, value: u32) -> Sample<u32> {
        Sample {
            point: ChainPoint {
                block,
                log_index: 0,
            },
            timestamp,
            value,
        }
    }

    fn arrival(block: u64, timestamp: u64) -> Arrival<()> {
        Arrival {
            point: ChainPoint {
                block,
                log_index: 0,
            },
            timestamp,
            tx: B256::ZERO,
            mark: (),
        }
    }

    #[test]
    fn a_series_refuses_disorder_and_answers_at_a_point_and_a_time() {
        let mut s = Series::default();
        assert!(s.push(at(10, 100, 1)));
        assert!(s.push(at(20, 200, 2)));
        assert!(!s.push(at(20, 200, 3)), "the same point is refused");
        assert!(!s.push(at(15, 150, 3)), "an earlier point is refused");
        assert_eq!(s.value(), Some(2));
        assert_eq!(
            s.at(ChainPoint {
                block: 15,
                log_index: 0
            })
            .map(|r| r.value),
            Some(1)
        );
        assert_eq!(
            s.at(ChainPoint {
                block: 9,
                log_index: 0
            }),
            None
        );
        assert_eq!(s.at_time(199).map(|r| r.value), Some(1));
        assert_eq!(s.at_time(200).map(|r| r.value), Some(2));
        let change = s.change_over(Window::seconds(100)).unwrap();
        assert_eq!((change.value.from, change.value.to), (1, 2));
        assert_eq!(change.moved_by.from.block, 10);
        assert_eq!(change.point.block, 20);
    }

    #[test]
    fn retention_keeps_the_window_and_the_sample_before_it() {
        let mut s = Series::default();
        s.retain(Retention::Last(Window::seconds(50)));
        for (b, t, v) in [(1, 10, 1), (2, 20, 2), (3, 70, 3), (4, 100, 4)] {
            s.push(at(b, t, v));
        }
        // The window is 50..=100; the sample at 20 is the one before it.
        let kept: Vec<u64> = s.samples().iter().map(|x| x.timestamp).collect();
        assert_eq!(kept, vec![20, 70, 100]);
        assert_eq!(
            s.at_time(60).map(|r| r.value),
            Some(2),
            "the window's start is answerable"
        );
        assert_eq!(s.since(5), None, "trimmed past it");
        assert_eq!(s.since(70).map(|w| w.len()), Some(2));
        assert_eq!(s.peak(Window::seconds(50)).map(|r| r.value), Some(4));
    }

    #[test]
    fn combining_appends_and_trims_as_one_fold_would() {
        let retention = Retention::Last(Window::seconds(50));
        let mut whole = Series::default();
        whole.retain(retention);
        let (mut a, mut b) = (Series::default(), Series::default());
        a.retain(retention);
        b.retain(retention);
        let samples = [(1, 10, 1), (2, 20, 2), (3, 70, 3), (4, 100, 4), (5, 130, 5)];
        for (i, (blk, t, v)) in samples.into_iter().enumerate() {
            whole.push(at(blk, t, v));
            if i < 2 {
                a.push(at(blk, t, v));
            } else {
                b.push(at(blk, t, v));
            }
        }
        a.combine(b);
        assert_eq!(a, whole);
    }

    #[test]
    fn arrivals_count_in_a_window_and_find_the_busiest() {
        let mut a = Arrivals::default();
        for (b, t) in [
            (1, 0),
            (2, 10),
            (3, 20),
            (4, 500),
            (5, 505),
            (6, 509),
            (7, 900),
        ] {
            a.push(arrival(b, t));
        }
        assert_eq!(a.count_in(Window::seconds(100)).map(|r| r.value), Some(1));
        assert_eq!(a.count_in(Window::seconds(400)).map(|r| r.value), Some(4));
        let busiest = a.busiest(Window::seconds(10)).unwrap();
        assert_eq!(busiest.len(), 3);
        assert_eq!(busiest[0].timestamp, 500);
        assert!(!a.push(arrival(7, 900)), "the same point is refused");
    }
}
