//! The one contract every fold over a market's events shares.
//!
//! A fold is a state that advances by one [`TapeEvent`] at a time. Fed from
//! the tape it is a batch computation; fed from the stamped feed it is a
//! live one; fed from an engine's synthetic events it is a counterfactual.
//! The same `apply` serves all three, which is what makes a fold written
//! against the past correct in the present.
//!
//! A fold that also [`combine`](Fold::combine)s is a monoid over segments of
//! the tape: cut at a block boundary, fold each segment on its own core,
//! merge. Every piece of state a market fold holds is one of three kinds
//! that combine associatively — a total the contract emitted, where the
//! later segment's wins; a sum of deltas, which add; or a first occurrence,
//! where the earlier segment's wins — so the law `fold(a ++ b) ==
//! combine(fold(a), fold(b))` holds, and a fold of any segment is a
//! checkpoint any later segment can start from.

mod series;
mod shapes;

use super::tape::{ChainPoint, TapeEvent};

pub use self::series::{
    Arrival, Arrivals, Change, Reading, Retention, Sample, Series, Span, Window,
};
pub use self::shapes::{First, Latest, Stated};

/// A deterministic fold over a market's events in chain order.
///
/// `apply` advances by one event; `fold` is `apply` over a tape; `combine`
/// merges the fold of the segment that follows this one. Implementations
/// may assert chain order and should allocate nothing on the common path
/// of `apply`.
pub trait Fold: Default {
    /// Advance by one event. Events arrive in chain order, the later
    /// segment's after this one's.
    fn apply(&mut self, event: &TapeEvent);

    /// Merge the fold of the segment after this one into this one, so
    /// that the result equals the fold of both segments in sequence.
    fn combine(&mut self, later: Self);

    /// The batch form: `apply` over every event, in order.
    fn fold<'a>(events: impl IntoIterator<Item = &'a TapeEvent>) -> Self {
        let mut fold = Self::default();
        for event in events {
            fold.apply(event);
        }
        fold
    }
}

/// A fold behind the chain-order guard: an event at or before the last
/// point applied is refused and counted, never handed on. Chain order is
/// the one assumption every sum in a fold rests on, and a driver that
/// breaks it is counted rather than trusted, in release builds as in
/// debug. Wrap any fold a driver feeds directly; `Replay` is one inside.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sequenced<F> {
    inner: F,
    last: Latest<ChainPoint>,
    refused: u32,
}

impl<F: Fold> Sequenced<F> {
    /// `inner` with nothing applied yet: the first event is taken whatever
    /// its point.
    pub fn new(inner: F) -> Self {
        Self {
            inner,
            last: Latest::default(),
            refused: 0,
        }
    }

    /// `inner` standing at `point`, as a fold seeded from the reads at a
    /// block does: events at or before it are already in and are refused.
    pub fn standing_at(inner: F, point: ChainPoint) -> Self {
        Self {
            inner,
            last: Latest::stated(point),
            refused: 0,
        }
    }

    /// Apply `event` if it is after the last point applied; whether it was.
    pub fn accept(&mut self, event: &TapeEvent) -> bool {
        let point = event.point();
        if self.last.get().is_some_and(|last| point <= last) {
            self.refused += 1;
            return false;
        }
        self.last.set(point);
        self.inner.apply(event);
        true
    }

    /// Where the fold stands: the last event's chain point.
    pub fn point(&self) -> Option<ChainPoint> {
        self.last.get()
    }

    /// Events refused for arriving at or before the fold's point.
    pub fn refused(&self) -> u32 {
        self.refused
    }

    /// The fold behind the guard.
    pub fn inner(&self) -> &F {
        &self.inner
    }

    /// The fold behind the guard, to set what is not an event: a retention,
    /// a configuration. Events go through [`accept`](Self::accept).
    pub(crate) fn inner_mut(&mut self) -> &mut F {
        &mut self.inner
    }

    /// The fold behind the guard, the guard dropped.
    pub fn into_inner(self) -> F {
        self.inner
    }
}

impl<F: Fold> Fold for Sequenced<F> {
    fn apply(&mut self, event: &TapeEvent) {
        self.accept(event);
    }

    /// Segments combine in order; a segment that starts at or before this
    /// one's point is a programming error, not data, and is asserted.
    fn combine(&mut self, later: Self) {
        debug_assert!(
            self.last
                .get()
                .zip(later.last.get())
                .is_none_or(|(a, b)| a < b),
            "segments combined out of order"
        );
        self.inner.combine(later.inner);
        self.last.combine(later.last);
        self.refused += later.refused;
    }
}
