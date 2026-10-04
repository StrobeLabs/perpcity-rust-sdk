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

use std::ops::AddAssign;

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
/// later segment's value wins when both have one. The first of the three
/// shapes a fold's state takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Latest<T>(Option<T>);

impl<T> Default for Latest<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<T: Copy> Latest<T> {
    /// A value already stated, as a read supplies one.
    pub const fn stated(value: T) -> Self {
        Self(Some(value))
    }

    /// The latest statement, replacing any before it.
    pub fn set(&mut self, value: T) {
        self.0 = Some(value);
    }

    /// The value as last stated, if ever.
    pub fn get(self) -> Option<T> {
        self.0
    }

    /// Merge the segment after this one: its statement wins if it made one.
    pub fn combine(&mut self, later: Self) {
        if later.0.is_some() {
            self.0 = later.0;
        }
    }
}

/// A value fixed by its first occurrence: the earlier segment's wins, and
/// a later occurrence is ignored. The second shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct First<T>(Option<T>);

impl<T> Default for First<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<T: Copy> First<T> {
    /// Record `value` unless one is already held.
    pub fn set(&mut self, value: T) {
        if self.0.is_none() {
            self.0 = Some(value);
        }
    }

    /// The first value recorded, if any.
    pub fn get(self) -> Option<T> {
        self.0
    }

    /// Whether a value has been recorded.
    pub fn is_set(self) -> bool {
        self.0.is_some()
    }

    /// Merge the segment after this one: its value counts only if this
    /// segment recorded none.
    pub fn combine(&mut self, later: Self) {
        if self.0.is_none() {
            self.0 = later.0;
        }
    }
}

/// A total the contract states outright, with what accrued since the
/// statement: a sum the caller keeps in `since`, of any type that adds. A
/// later statement replaces both; a segment without one adds its accruals
/// to the one before. The third shape, and the one the solvency books take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stated<T, S> {
    value: Option<T>,
    /// Whether this segment holds a statement, which decides how `since`
    /// combines.
    stated: bool,
    /// What accrued since the statement, the caller's to add to.
    pub since: S,
}

impl<T, S: Default> Default for Stated<T, S> {
    fn default() -> Self {
        Self {
            value: None,
            stated: false,
            since: S::default(),
        }
    }
}

impl<T: Copy, S: Default + AddAssign> Stated<T, S> {
    /// A total stated with nothing accrued since: zero before a market's
    /// first event, or what a read returned.
    pub fn with(value: T) -> Self {
        Self {
            value: Some(value),
            stated: true,
            since: S::default(),
        }
    }

    /// A new statement: the total is `value` and nothing has accrued since.
    pub fn state(&mut self, value: T) {
        self.value = Some(value);
        self.stated = true;
        self.since = S::default();
    }

    /// The total as last stated, if ever.
    pub fn value(&self) -> Option<T> {
        self.value
    }

    /// Merge the segment after this one: its statement replaces this one's
    /// total and accruals; without one, its accruals add to these.
    pub fn combine(&mut self, later: Self) {
        if later.stated {
            *self = later;
        } else {
            self.since += later.since;
        }
    }
}
