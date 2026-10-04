//! The two sides of a market, and a pair of anything keyed by them.
//!
//! Neither is a number. A [`Side`] is the key every directional quantity
//! answers to — capacity, open interest, utilization, the sign of an
//! exposure — and [`PerSide`] is two of a thing, one per side, so that a
//! struct carrying `long_x` and `short_x` as separate fields carries one
//! field a side can index instead. Before this a strategy wrote `is_long`
//! and branched, and every pair of fields was two names nothing related.

use std::fmt;
use std::ops::Add;

use serde::{Deserialize, Serialize};

use super::amount::{PerpAtoms, PerpDelta};

/// A taker direction: a long gains when the price rises, a short when it
/// falls. The sign of a [`PerpDelta`] is the same fact as a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    /// Long exposure: a positive perp delta.
    Long,
    /// Short exposure: a negative perp delta.
    Short,
}

impl Side {
    /// Both, long first: the order the contract's structs name them in.
    pub const BOTH: [Self; 2] = [Self::Long, Self::Short];

    /// The other side: a long's counterparty, a short's.
    pub const fn opposite(self) -> Self {
        match self {
            Self::Long => Self::Short,
            Self::Short => Self::Long,
        }
    }

    /// `Long` when `long` holds, else `Short`: the side a boolean judgement
    /// — buy or sell, mark below the index or above — names.
    pub const fn long_if(long: bool) -> Self {
        if long { Self::Long } else { Self::Short }
    }

    /// `+1` for long, `-1` for short: the direction as a factor, for tick
    /// offsets and the like.
    pub const fn sign(self) -> i32 {
        match self {
            Self::Long => 1,
            Self::Short => -1,
        }
    }

    /// `size` taken on this side, as the signed exposure the position
    /// records.
    pub fn exposure(self, size: PerpAtoms) -> PerpDelta {
        let long = PerpDelta::from(size);
        match self {
            Self::Long => long,
            Self::Short => -long,
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Long => "long",
            Self::Short => "short",
        })
    }
}

impl PerpDelta {
    /// Which side this exposure is on; `None` for no exposure at all.
    pub fn side(self) -> Option<Side> {
        match self.atoms().signum() {
            1 => Some(Side::Long),
            -1 => Some(Side::Short),
            _ => None,
        }
    }
}

/// One `T` per side.
///
/// The fields are named as the contract names them, so a struct that held
/// `long` and `short` serialises the same after adopting this; a side is
/// read with [`on`](Self::on) rather than by naming its field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[must_use]
pub struct PerSide<T> {
    /// The long side's.
    pub long: T,
    /// The short side's.
    pub short: T,
}

impl<T> PerSide<T> {
    /// From the two values, long first.
    pub const fn new(long: T, short: T) -> Self {
        Self { long, short }
    }

    /// The same value on both sides.
    pub fn uniform(value: T) -> Self
    where
        T: Clone,
    {
        Self {
            long: value.clone(),
            short: value,
        }
    }

    /// The value on one side.
    pub const fn on(&self, side: Side) -> &T {
        match side {
            Side::Long => &self.long,
            Side::Short => &self.short,
        }
    }

    /// The value on one side, to change.
    pub fn on_mut(&mut self, side: Side) -> &mut T {
        match side {
            Side::Long => &mut self.long,
            Side::Short => &mut self.short,
        }
    }

    /// Each side's value through `f`.
    pub fn map<U>(self, mut f: impl FnMut(T) -> U) -> PerSide<U> {
        PerSide {
            long: f(self.long),
            short: f(self.short),
        }
    }

    /// Each side's value beside `other`'s on the same side.
    pub fn zip<U>(self, other: PerSide<U>) -> PerSide<(T, U)> {
        PerSide {
            long: (self.long, other.long),
            short: (self.short, other.short),
        }
    }

    /// Both sides added: a market's gross figure.
    pub fn total(self) -> T
    where
        T: Add<Output = T>,
    {
        self.long + self.short
    }

    /// The two values with their sides, long first.
    pub fn iter(&self) -> impl Iterator<Item = (Side, &T)> {
        [(Side::Long, &self.long), (Side::Short, &self.short)].into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A judgement names a side.
    #[test]
    fn a_judgement_names_a_side() {
        assert_eq!(Side::long_if(true), Side::Long);
        assert_eq!(Side::long_if(false), Side::Short);
    }

    /// A side is the sign of an exposure, and each is the other's opposite.
    #[test]
    fn a_side_is_the_sign_of_an_exposure() {
        assert_eq!(PerpDelta::new(5).side(), Some(Side::Long));
        assert_eq!(PerpDelta::new(-5).side(), Some(Side::Short));
        assert_eq!(PerpDelta::ZERO.side(), None, "no exposure is on no side");
        assert_eq!(Side::Short.exposure(PerpAtoms::new(5)), PerpDelta::new(-5));
        assert_eq!(
            Side::Long.exposure(PerpAtoms::new(5)).side(),
            Some(Side::Long)
        );
        assert_eq!(Side::Long.opposite(), Side::Short);
        assert_eq!(Side::Short.opposite().sign(), 1);
        assert_eq!(Side::BOTH, [Side::Long, Side::Short]);
    }

    /// A pair reads by side, maps as one, and sums across the market.
    #[test]
    fn a_pair_is_keyed_by_side() {
        let mut oi = PerSide::new(PerpAtoms::new(10), PerpAtoms::new(4));
        assert_eq!(*oi.on(Side::Short), PerpAtoms::new(4));
        assert_eq!(oi.total(), PerpAtoms::new(14));
        *oi.on_mut(Side::Short) = PerpAtoms::new(6);
        assert_eq!(oi.short, PerpAtoms::new(6));

        let cap = PerSide::uniform(PerpAtoms::new(12));
        let headroom = cap.zip(oi).map(|(c, o)| c.saturating_sub(o));
        assert_eq!(headroom, PerSide::new(PerpAtoms::new(2), PerpAtoms::new(6)));
        assert_eq!(
            headroom.iter().map(|(side, _)| side).collect::<Vec<_>>(),
            Side::BOTH
        );
    }

    /// The wire form is the contract's two named fields, so a struct that
    /// held `long` and `short` reads back after adopting the pair.
    #[test]
    fn the_wire_form_is_the_two_named_fields() {
        let pair = PerSide::new(1u8, 2u8);
        assert_eq!(
            serde_json::to_string(&pair).unwrap(),
            r#"{"long":1,"short":2}"#
        );
        assert_eq!(serde_json::to_string(&Side::Long).unwrap(), r#""Long""#);
    }
}
