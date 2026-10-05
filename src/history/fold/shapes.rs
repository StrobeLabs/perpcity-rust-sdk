//! The three shapes a fold's state takes, each with the `combine` its law
//! needs and nothing else.

use std::ops::AddAssign;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_latest_statement_stands_and_a_segment_without_one_leaves_it() {
        let mut earlier = Latest::stated(1);
        earlier.combine(Latest::default());
        assert_eq!(earlier.get(), Some(1), "a segment that learned nothing");
        let mut later = Latest::default();
        later.set(2);
        earlier.combine(later);
        assert_eq!(earlier.get(), Some(2), "the later segment's wins");
        let mut unknown: Latest<u32> = Latest::default();
        unknown.combine(Latest::stated(3));
        assert_eq!(unknown.get(), Some(3));
    }

    #[test]
    fn the_first_occurrence_stands_whichever_segment_saw_it() {
        let mut earlier = First::default();
        earlier.set(1);
        earlier.set(2);
        assert_eq!(earlier.get(), Some(1), "a later occurrence is ignored");
        let mut later = First::default();
        later.set(3);
        earlier.combine(later);
        assert_eq!(earlier.get(), Some(1), "the earlier segment's wins");
        let mut unknown: First<u32> = First::default();
        assert!(!unknown.is_set());
        let mut later = First::default();
        later.set(4);
        unknown.combine(later);
        assert_eq!(
            unknown.get(),
            Some(4),
            "a segment that saw nothing takes the later's"
        );
    }

    #[test]
    fn a_statement_replaces_the_accruals_and_a_segment_without_one_adds_them() {
        let mut books: Stated<u32, u32> = Stated::with(100);
        books.since += 5;
        let mut unstated: Stated<u32, u32> = Stated::default();
        unstated.since += 7;
        books.combine(unstated);
        assert_eq!(
            (books.value(), books.since),
            (Some(100), 12),
            "accruals add"
        );
        let mut restated: Stated<u32, u32> = Stated::default();
        restated.state(200);
        restated.since += 1;
        books.combine(restated);
        assert_eq!(
            (books.value(), books.since),
            (Some(200), 1),
            "a later statement replaces the total and the accruals"
        );
        let mut segment: Stated<u32, u32> = Stated::default();
        segment.since += 3;
        assert_eq!(
            segment.value(),
            None,
            "a segment never told the total does not invent it"
        );
    }
}
