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
//! Each knows how far it is known, which is not its last sample: a fold
//! advances every series it holds on every event it reads, and a driver
//! advances them over a range it read without events. Up to that point,
//! no sample means nothing happened; past it, nothing is known. A
//! windowed question ends there, so a quiet window is an answer, zero
//! arrivals or an unchanged value, and an alarm on it clears. A question
//! about the past is asked [`until`](Series::until) a point, and ends
//! there instead.
//!
//! A [`Reading`] is what a question returns: the value in its units, the
//! point it holds at, and the span of samples it was computed from, so an
//! alarm can name what moved it and a forensic bin can reproduce it from
//! the tape.

use std::time::Duration;

use alloy::primitives::B256;

use crate::history::tape::{ChainPoint, OwnershipLog, Positioned, Wallets};

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
    /// Where it holds: the sample's point for a value, the window's end
    /// for a question over a window.
    pub point: ChainPoint,
    /// The block timestamp there.
    pub timestamp: u64,
    /// The samples it was computed from; `None` for a window nothing
    /// arrived in.
    pub moved_by: Option<Span>,
}

impl<T> Reading<T> {
    fn of(value: T, first: ChainPoint, last: &Sample<impl Copy>) -> Self {
        Self {
            value,
            point: last.point,
            timestamp: last.timestamp,
            moved_by: Some(Span {
                from: first,
                to: last.point,
            }),
        }
    }

    fn ending(value: T, end: Sample<()>, moved_by: Option<Span>) -> Self {
        Self {
            value,
            point: end.point,
            timestamp: end.timestamp,
            moved_by,
        }
    }
}

/// A value then and now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change<T> {
    /// The value at the start of the window.
    pub from: T,
    /// The value at its end.
    pub to: T,
}

/// A series or a point process asked about as of `end`: its windows end
/// there. From [`Series::until`] or [`Arrivals::until`].
#[derive(Debug, Clone, Copy)]
pub struct Until<'a, S> {
    of: &'a S,
    end: Sample<()>,
}

/// The later of two points known through.
fn later(a: Option<Sample<()>>, b: Option<Sample<()>>) -> Option<Sample<()>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if b.point > a.point { b } else { a }),
        (a, b) => a.or(b),
    }
}

/// A value that holds between updates, in chain order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Series<T> {
    samples: Vec<Sample<T>>,
    retention: Retention,
    /// Whether retention has dropped samples, so an answer about a time
    /// before the earliest sample is `None` rather than the earliest.
    trimmed: bool,
    /// The last point the series is known through, at or after its last
    /// sample.
    through: Option<Sample<()>>,
}

impl<T> Default for Series<T> {
    fn default() -> Self {
        Self {
            samples: Vec::new(),
            retention: Retention::All,
            trimmed: false,
            through: None,
        }
    }
}

impl<T: Copy> Series<T> {
    /// A series holding one statement, as a read supplies it, known
    /// through its point.
    pub fn stated(sample: Sample<T>) -> Self {
        Self {
            samples: vec![sample],
            through: Some(sample.with(())),
            ..Self::default()
        }
    }

    /// Keep only `retention` from here on, trimming now.
    pub fn retain(&mut self, retention: Retention) {
        self.retention = retention;
        self.trim();
    }

    /// Record `sample` if it is after the last; whether it was taken. The
    /// series is then known through it.
    pub fn push(&mut self, sample: Sample<T>) -> bool {
        if self
            .samples
            .last()
            .is_some_and(|last| sample.point <= last.point)
        {
            return false;
        }
        self.samples.push(sample);
        self.advance(sample.with(()));
        true
    }

    /// Known through `at` with nothing stated since the last sample: a fold
    /// advances every series it holds on every event it reads, and a
    /// driver on a range it read without events. A point at or before the
    /// one known through changes nothing.
    pub fn advance(&mut self, at: Sample<()>) {
        self.through = later(self.through, Some(at));
        self.trim();
    }

    /// The last point the series is known through.
    pub fn through(&self) -> Option<Sample<()>> {
        self.through
    }

    /// The series asked about as of `end`: for a question about the past,
    /// at a point taken from the tape. Past the point it is known
    /// through, it answers nothing.
    pub fn until(&self, end: Sample<()>) -> Until<'_, Self> {
        Until { of: self, end }
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

    /// The samples inside the window ending where the series is known
    /// through.
    pub fn window(&self, window: Window) -> &[Sample<T>] {
        self.through
            .map_or(&[], |end| self.until(end).window(window))
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

    /// The value at the start of the window ending where the series is
    /// known through, and at its end.
    pub fn change_over(&self, window: Window) -> Option<Reading<Change<T>>> {
        self.until(self.through?).change_over(window)
    }

    /// Append the segment after this one.
    pub fn combine(&mut self, later: Self) {
        for sample in later.samples {
            self.push(sample);
        }
        self.trimmed |= later.trimmed;
        if let Some(through) = later.through {
            self.advance(through);
        }
    }

    /// Drop what retention does not keep: everything before the window
    /// ending where the series is known through, except the last sample
    /// before it.
    fn trim(&mut self) {
        let Retention::Last(window) = self.retention else {
            return;
        };
        let Some(through) = self.through else {
            return;
        };
        let from = through.timestamp.saturating_sub(window.as_secs());
        let first_inside = self.samples.partition_point(|s| s.timestamp < from);
        let keep_from = first_inside.saturating_sub(1);
        if keep_from > 0 {
            self.samples.drain(..keep_from);
            self.trimmed = true;
        }
    }
}

impl<T: Copy + PartialOrd> Series<T> {
    /// The largest value the series held in the window ending where it is
    /// known through.
    pub fn peak(&self, window: Window) -> Option<Reading<T>> {
        self.until(self.through?).peak(window)
    }
}

impl<'a, T: Copy> Until<'a, Series<T>> {
    /// Whether the series is known through the end.
    fn known(&self) -> bool {
        self.of
            .through
            .is_some_and(|through| self.end.point <= through.point)
    }

    /// The samples at or before the end, and the index of the first inside
    /// the window.
    fn bounds(&self, window: Window) -> (&'a [Sample<T>], usize) {
        let samples = &self.of.samples;
        let before_end = &samples[..samples.partition_point(|s| s.point <= self.end.point)];
        let from = self.end.timestamp.saturating_sub(window.as_secs());
        (
            before_end,
            before_end.partition_point(|s| s.timestamp < from),
        )
    }

    /// The samples inside the window ending at the end; none past the
    /// point the series is known through.
    pub fn window(&self, window: Window) -> &'a [Sample<T>] {
        if !self.known() {
            return &[];
        }
        let (samples, start) = self.bounds(window);
        &samples[start..]
    }

    /// The value at the start of the window and at its end; `None` past the
    /// point the series is known through, or before its first sample kept.
    pub fn change_over(&self, window: Window) -> Option<Reading<Change<T>>> {
        if !self.known() {
            return None;
        }
        let from = self.end.timestamp.saturating_sub(window.as_secs());
        let then = self.of.at_time(from)?;
        let now = self.of.at(self.end.point)?;
        Some(Reading::ending(
            Change {
                from: then.value,
                to: now.value,
            },
            self.end,
            Some(Span {
                from: then.point,
                to: now.point,
            }),
        ))
    }
}

impl<T: Copy + PartialOrd> Until<'_, Series<T>> {
    /// The largest value held in the window ending at the end, counting the
    /// one standing at its start; the span runs to the sample that set it.
    pub fn peak(&self, window: Window) -> Option<Reading<T>> {
        if !self.known() {
            return None;
        }
        let (samples, start) = self.bounds(window);
        let from = self.end.timestamp.saturating_sub(window.as_secs());
        let standing_at_start = start > 0 && samples.get(start).is_none_or(|s| s.timestamp > from);
        let held = &samples[if standing_at_start { start - 1 } else { start }..];
        let first = held.first()?.point;
        let peak = held.iter().fold(None::<&Sample<T>>, |best, s| match best {
            Some(b) if b.value >= s.value => Some(b),
            _ => Some(s),
        })?;
        Some(Reading::ending(
            peak.value,
            self.end,
            Some(Span {
                from: first,
                to: peak.point,
            }),
        ))
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
///
/// It knows the span it is complete over: from where its fold began
/// watching to where it is known through. A window reaching outside that
/// span is not answered, since an arrival there may be missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arrivals<M> {
    arrivals: Vec<Arrival<M>>,
    retention: Retention,
    /// The first block time every arrival is kept from.
    complete_from: Option<u64>,
    /// The last point the process is known through.
    through: Option<Sample<()>>,
}

impl<M> Default for Arrivals<M> {
    /// Watching from the first point it is told of, exclusive: a fold of a
    /// segment does not know what arrived earlier in that block time.
    fn default() -> Self {
        Self {
            arrivals: Vec::new(),
            retention: Retention::All,
            complete_from: None,
            through: None,
        }
    }
}

impl<M> Arrivals<M> {
    /// Before a market's first event: complete from the start.
    pub fn from_genesis() -> Self {
        Self {
            complete_from: Some(0),
            ..Self::default()
        }
    }

    /// Watching from the end of a seed's block: complete after its block
    /// time, known through `at`.
    pub fn after(at: Sample<()>) -> Self {
        Self {
            complete_from: Some(at.timestamp + 1),
            through: Some(at),
            ..Self::default()
        }
    }

    /// Keep only `retention` from here on, trimming now.
    pub fn retain(&mut self, retention: Retention) {
        self.retention = retention;
        self.trim();
    }

    /// Record `arrival` if it is after the last; whether it was taken. The
    /// process is then known through it.
    pub fn push(&mut self, arrival: Arrival<M>) -> bool {
        if self
            .arrivals
            .last()
            .is_some_and(|last| arrival.point <= last.point)
        {
            return false;
        }
        let at = Sample {
            point: arrival.point,
            timestamp: arrival.timestamp,
            value: (),
        };
        self.arrivals.push(arrival);
        self.advance(at);
        true
    }

    /// Known through `at` with nothing arrived since the last: a fold
    /// advances every process it holds on every event it reads, and a
    /// driver on a range it read without events. A point at or before the
    /// one known through changes nothing.
    pub fn advance(&mut self, at: Sample<()>) {
        self.complete_from.get_or_insert(at.timestamp + 1);
        self.through = later(self.through, Some(at));
        self.trim();
    }

    /// The last point the process is known through.
    pub fn through(&self) -> Option<Sample<()>> {
        self.through
    }

    /// The process asked about as of `end`: for a question about the past,
    /// at a point taken from the tape. Past the point it is known
    /// through, it answers nothing.
    pub fn until(&self, end: Sample<()>) -> Until<'_, Self> {
        Until { of: self, end }
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

    /// The arrivals inside the window ending where the process is known
    /// through; `None` for a window it is not complete over.
    pub fn window(&self, window: Window) -> Option<&[Arrival<M>]> {
        self.until(self.through?).window(window)
    }

    /// How many arrived in the window ending where the process is known
    /// through, zero for a quiet one; `None` for a window it is not
    /// complete over.
    pub fn count_in(&self, window: Window) -> Option<Reading<u32>> {
        self.until(self.through?).count_in(window)
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
        if self.complete_from.is_none() {
            self.complete_from = later.complete_from;
        }
        for arrival in later.arrivals {
            self.push(arrival);
        }
        if let Some(through) = later.through {
            self.advance(through);
        }
    }

    /// The arrivals on positions one of `wallets` held when they arrived,
    /// as `custody` records it, under this retention: a market's arrivals
    /// scoped to a cohort or to one agent. One lookup per arrival.
    pub fn by(&self, wallets: &Wallets, custody: &OwnershipLog) -> Self
    where
        M: Positioned + Clone,
    {
        Self {
            arrivals: self
                .arrivals
                .iter()
                .filter(|arrival| custody.held_by_at(arrival.mark.pos_id(), wallets, arrival.point))
                .cloned()
                .collect(),
            retention: self.retention,
            complete_from: self.complete_from,
            through: self.through,
        }
    }

    /// Drop what retention does not keep, everything before the window
    /// ending where the process is known through, and stop answering for
    /// it.
    fn trim(&mut self) {
        let Retention::Last(window) = self.retention else {
            return;
        };
        let Some(through) = self.through else {
            return;
        };
        let from = through.timestamp.saturating_sub(window.as_secs());
        let keep_from = self.arrivals.partition_point(|a| a.timestamp < from);
        self.arrivals.drain(..keep_from);
        self.complete_from = self.complete_from.map(|start| start.max(from));
    }
}

impl<'a, M> Until<'a, Arrivals<M>> {
    /// The arrivals inside the window ending at the end; `None` past the
    /// point the process is known through, or for a window starting before
    /// it is complete.
    pub fn window(&self, window: Window) -> Option<&'a [Arrival<M>]> {
        let from = self.end.timestamp.saturating_sub(window.as_secs());
        let complete = self.of.complete_from.is_some_and(|start| from >= start)
            && self
                .of
                .through
                .is_some_and(|through| self.end.point <= through.point);
        if !complete {
            return None;
        }
        let arrivals = &self.of.arrivals;
        let end = arrivals.partition_point(|a| a.point <= self.end.point);
        let start = arrivals[..end].partition_point(|a| a.timestamp < from);
        Some(&arrivals[start..end])
    }

    /// How many arrived in the window ending at the end, zero for a quiet
    /// one; `None` where [`window`](Self::window) is.
    pub fn count_in(&self, window: Window) -> Option<Reading<u32>> {
        let inside = self.window(window)?;
        let moved_by = inside.first().zip(inside.last()).map(|(first, last)| Span {
            from: first.point,
            to: last.point,
        });
        Some(Reading::ending(inside.len() as u32, self.end, moved_by))
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
        assert_eq!(change.moved_by.map(|span| span.from.block), Some(10));
        assert_eq!(change.point.block, 20);
    }

    fn through(block: u64, timestamp: u64) -> Sample<()> {
        at(block, timestamp, 0).with(())
    }

    #[test]
    fn a_series_known_past_its_last_sample_ends_its_windows_there() {
        let mut s = Series::default();
        s.push(at(10, 100, 1));
        s.push(at(20, 200, 5));
        s.advance(through(90, 900));
        assert_eq!(s.through(), Some(through(90, 900)));

        let quiet = s.change_over(Window::seconds(100)).unwrap();
        assert_eq!(
            (quiet.value.from, quiet.value.to),
            (5, 5),
            "nothing moved in the last hundred seconds"
        );
        assert_eq!(quiet.point.block, 90, "the reading holds where it is known");
        assert!(s.window(Window::seconds(100)).is_empty());
        assert_eq!(
            s.peak(Window::seconds(100)).map(|r| r.value),
            Some(5),
            "the value standing at the window's start held through it"
        );

        let then = s.until(through(20, 200));
        assert_eq!(
            then.change_over(Window::seconds(100)).map(|r| r.value),
            Some(Change { from: 1, to: 5 }),
            "asked as of the second sample, as before the advance"
        );
        assert_eq!(
            s.until(through(91, 901)).change_over(Window::seconds(100)),
            None,
            "past the point it is known through, nothing"
        );
    }

    #[test]
    fn a_quiet_window_counts_zero_and_an_incomplete_one_is_not_answered() {
        let mut a = Arrivals::from_genesis();
        a.push(arrival(1, 100));
        a.push(arrival(2, 110));
        assert_eq!(a.count_in(Window::seconds(60)).map(|r| r.value), Some(2));

        a.advance(through(50, 500));
        let quiet = a.count_in(Window::seconds(60)).unwrap();
        assert_eq!(quiet.value, 0, "the burst has left the window");
        assert_eq!(quiet.moved_by, None);
        assert_eq!(quiet.point.block, 50);

        let past = a
            .until(through(2, 110))
            .count_in(Window::seconds(60))
            .unwrap();
        assert_eq!(past.value, 2, "the burst, asked about as of its end");
        assert_eq!(past.moved_by.map(|span| span.from.block), Some(1));
        assert_eq!(
            a.until(through(51, 501)).count_in(Window::seconds(60)),
            None,
            "past the point it is known through"
        );

        let seeded = Arrivals::<()>::after(through(9, 1_000));
        assert_eq!(
            seeded.count_in(Window::seconds(60)),
            None,
            "a window reaching back past the seed may be missing arrivals"
        );
        let mut later = seeded.clone();
        later.advance(through(20, 1_100));
        assert_eq!(
            later.count_in(Window::seconds(60)).map(|r| r.value),
            Some(0)
        );

        let mut kept = Arrivals::from_genesis();
        kept.retain(Retention::Last(Window::seconds(50)));
        kept.push(arrival(1, 100));
        kept.advance(through(5, 200));
        assert_eq!(kept.count_in(Window::seconds(50)).map(|r| r.value), Some(0));
        assert_eq!(
            kept.count_in(Window::seconds(150)),
            None,
            "retention dropped what a longer window would count"
        );
    }

    #[test]
    fn arrivals_known_through_combine_as_one_fold_would() {
        let retention = Retention::Last(Window::seconds(50));
        let mut whole = Arrivals::default();
        whole.retain(retention);
        let (mut a, mut b) = (Arrivals::default(), Arrivals::default());
        a.retain(retention);
        b.retain(retention);
        for (i, (blk, t)) in [(1, 10), (2, 20), (3, 70), (4, 100)]
            .into_iter()
            .enumerate()
        {
            whole.push(arrival(blk, t));
            if i < 2 {
                a.push(arrival(blk, t));
            } else {
                b.push(arrival(blk, t));
            }
        }
        whole.advance(through(9, 400));
        b.advance(through(9, 400));
        a.combine(b);
        assert_eq!(a, whole);
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
