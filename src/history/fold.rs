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

use super::tape::TapeEvent;

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

/// A total the contract emits whole: the latest statement stands, so the
/// later segment's value wins when both have one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Latest<T>(Option<T>);

impl<T> Default for Latest<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<T: Copy> Latest<T> {
    pub(crate) const fn stated(value: T) -> Self {
        Self(Some(value))
    }

    pub(crate) fn set(&mut self, value: T) {
        self.0 = Some(value);
    }

    pub(crate) fn get(self) -> Option<T> {
        self.0
    }

    pub(crate) fn combine(&mut self, later: Self) {
        if later.0.is_some() {
            self.0 = later.0;
        }
    }
}

/// A value fixed by its first occurrence: the earlier segment's wins, and
/// a later occurrence is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct First<T>(Option<T>);

impl<T> Default for First<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<T: Copy> First<T> {
    /// Record `value` unless one is already held.
    pub(crate) fn set(&mut self, value: T) {
        if self.0.is_none() {
            self.0 = Some(value);
        }
    }

    pub(crate) fn get(self) -> Option<T> {
        self.0
    }

    pub(crate) fn is_set(self) -> bool {
        self.0.is_some()
    }

    pub(crate) fn combine(&mut self, later: Self) {
        if self.0.is_none() {
            self.0 = later.0;
        }
    }
}
