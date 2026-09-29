//! A maker's range: the geometry `makerDetails` stores, [`MakerRange`].

use serde::{Deserialize, Serialize};

/// A maker position's liquidity range as the contract stores it: the tick
/// bounds and the liquidity standing between them.
///
/// The one type for a band's geometry wherever it appears — read back
/// from chain ([`StateAt::maker_range`](crate::StateAt::maker_range)),
/// sized before it opens, or tracked after it did — so
/// [`band_capacity`](crate::band_capacity) takes it whole.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MakerRange {
    /// Lower tick bound.
    pub tick_lower: i32,
    /// Upper tick bound.
    pub tick_upper: i32,
    /// Liquidity standing in the range, in the pool's liquidity units.
    pub liquidity: u128,
}

impl MakerRange {
    /// A range over `[tick_lower, tick_upper]` holding `liquidity`.
    pub const fn new(tick_lower: i32, tick_upper: i32, liquidity: u128) -> Self {
        Self {
            tick_lower,
            tick_upper,
            liquidity,
        }
    }

    /// Whether the range's liquidity is active at `tick`: `tick_lower <=
    /// tick < tick_upper`, the half-open interval V4 activates it on.
    pub const fn contains(&self, tick: i32) -> bool {
        self.tick_lower <= tick && tick < self.tick_upper
    }

    /// Whether `tick` is exactly one of the bounds.
    ///
    /// The deployed contracts corrupt a tick's funding accumulator when a
    /// swap stops exactly on it, so this is the question a funding audit
    /// asks of a range.
    pub const fn is_boundary(&self, tick: i32) -> bool {
        tick == self.tick_lower || tick == self.tick_upper
    }

    /// The range's width in ticks.
    pub const fn width(&self) -> i32 {
        self.tick_upper - self.tick_lower
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RANGE: MakerRange = MakerRange::new(100, 200, 7);

    #[test]
    fn contains_is_half_open_as_v4_activates_liquidity() {
        assert!(RANGE.contains(100), "active from the lower bound");
        assert!(RANGE.contains(199));
        assert!(!RANGE.contains(200), "inactive at the upper bound");
        assert!(!RANGE.contains(99));
    }

    #[test]
    fn a_range_knows_its_own_boundaries() {
        assert!(RANGE.is_boundary(100) && RANGE.is_boundary(200));
        assert!(!RANGE.is_boundary(150), "inside is not on");
        assert!(!RANGE.is_boundary(201));
    }

    #[test]
    fn width_is_the_tick_span() {
        assert_eq!(RANGE.width(), 100);
        assert_eq!(MakerRange::new(-60, 60, 0).width(), 120);
    }
}
