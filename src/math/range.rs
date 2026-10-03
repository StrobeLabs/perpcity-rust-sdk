//! A maker's geometry: the tick interval a range occupies ([`TickRange`])
//! and the liquidity standing in it ([`MakerBand`]).
//!
//! This is the root the maker math hangs off. A [`TickRange`] is valid by
//! construction, so [`liquidity`](crate::math::liquidity) and
//! [`capacity`](crate::math::capacity) take one and trust it: an invalid
//! range fails at the boundary it entered — a chain read, a config, an
//! event — never somewhere inside the arithmetic.

use serde::{Deserialize, Serialize};

use crate::constants::{MAX_TICK, MIN_TICK, Q96, TICK_SPACING};
use crate::errors::ValidationError;
use crate::math::tick::{align_tick_down, align_tick_up, get_sqrt_ratio_at_tick};
use crate::units::{LUnits, Price, SqrtPrice};

/// A tick interval `[lower, upper)`, valid by construction: `lower <
/// upper`, both within the V4 domain `[MIN_TICK, MAX_TICK]`.
///
/// The one place a range is checked. The fields are private so the
/// invariant holds for the type's lifetime; a deserialised range is
/// checked the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawTickRange", into = "RawTickRange")]
pub struct TickRange {
    lower: i32,
    upper: i32,
}

impl TickRange {
    /// The range `[lower, upper)`.
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidTickRange`] if `lower >= upper` or either
    /// tick is outside `[MIN_TICK, MAX_TICK]`.
    pub fn new(lower: i32, upper: i32) -> Result<Self, ValidationError> {
        if lower >= upper || lower < MIN_TICK || upper > MAX_TICK {
            return Err(ValidationError::InvalidTickRange { lower, upper });
        }
        Ok(Self { lower, upper })
    }

    /// The narrowest range on the pool's spacing that encloses
    /// `[lower, upper]`: the lower price's tick aligned down, the upper's
    /// aligned up. Two prices a strategy computed — a landing zone either
    /// side of the index, a corridor around the mark — become the band the
    /// pool will accept.
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidPrice`] when a price has no tick in the
    /// pool's domain, and [`ValidationError::InvalidTickRange`] when the
    /// aligned ticks do not make a range.
    pub fn between(lower: Price, upper: Price) -> Result<Self, ValidationError> {
        Self::new(
            align_tick_down(lower.tick()?, TICK_SPACING),
            align_tick_up(upper.tick()?, TICK_SPACING),
        )
    }

    /// The range's geometric centre, `√(P_lo · P_hi)`: the price at which
    /// a band's two legs are worth the same, exact in Q96.
    pub fn geomean(&self) -> Price {
        let (lo, hi) = self.sqrt_bounds();
        Price::from_x96(lo.x96() * hi.x96() / Q96)
    }

    /// Lower tick bound.
    pub const fn lower(&self) -> i32 {
        self.lower
    }

    /// Upper tick bound.
    pub const fn upper(&self) -> i32 {
        self.upper
    }

    /// Whether the range's liquidity is active at `tick`: `lower <= tick
    /// < upper`, the half-open interval V4 activates it on.
    pub const fn contains(&self, tick: i32) -> bool {
        self.lower <= tick && tick < self.upper
    }

    /// Whether `tick` is exactly one of the bounds.
    ///
    /// The deployed contracts corrupt a tick's funding accumulator when a
    /// swap stops exactly on it, so this is the question a funding audit
    /// asks of a range.
    pub const fn is_boundary(&self, tick: i32) -> bool {
        tick == self.lower || tick == self.upper
    }

    /// The range's width in ticks.
    pub const fn width(&self) -> i32 {
        self.upper - self.lower
    }

    /// The sqrt prices at the bounds — what the V4 math works in.
    pub fn sqrt_bounds(&self) -> (SqrtPrice, SqrtPrice) {
        let at =
            |tick| get_sqrt_ratio_at_tick(tick).expect("a TickRange's ticks are in the V4 domain");
        (at(self.lower), at(self.upper))
    }
}

/// `TickRange`'s wire shape: the two ticks, checked on the way in.
#[derive(Serialize, Deserialize)]
struct RawTickRange {
    lower: i32,
    upper: i32,
}

impl TryFrom<RawTickRange> for TickRange {
    type Error = ValidationError;

    fn try_from(raw: RawTickRange) -> Result<Self, Self::Error> {
        Self::new(raw.lower, raw.upper)
    }
}

impl From<TickRange> for RawTickRange {
    fn from(range: TickRange) -> Self {
        Self {
            lower: range.lower,
            upper: range.upper,
        }
    }
}

/// A maker's band: a range and the liquidity standing in it, the shape
/// `makerDetails` stores.
///
/// The one type for a band wherever it appears — read back from chain
/// ([`StateAt::maker_band`](crate::StateAt::maker_band)), sized before
/// it opens, or tracked after it did — so
/// [`band_capacity`](crate::band_capacity) and
/// [`band_amounts`](crate::math::liquidity::band_amounts) take it whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MakerBand {
    /// The range the liquidity stands in.
    pub range: TickRange,
    /// Liquidity in the range, in the pool's liquidity units.
    pub liquidity: LUnits,
}

impl MakerBand {
    /// `liquidity` standing in `range`.
    pub const fn new(range: TickRange, liquidity: LUnits) -> Self {
        Self { range, liquidity }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::Share;

    /// Two prices become the narrowest band on the spacing that holds
    /// them, and the band's centre is the geometric mean of its ends.
    #[test]
    fn a_range_is_built_between_two_prices() {
        let index = Price::at_tick(46_035).unwrap();
        let zone = Share::try_from(0.01).unwrap();
        let built = TickRange::between(
            index * (1.0 - zone.fraction()),
            index * (1.0 + zone.fraction()),
        )
        .unwrap();
        assert_eq!(built.lower() % TICK_SPACING, 0);
        assert_eq!(built.upper() % TICK_SPACING, 0);
        assert!(
            built.contains(46_035),
            "{built:?} does not hold the index's tick"
        );
        // One percent each way is about 100 ticks each way, then widened to
        // the spacing: 200 to 260 ticks.
        assert!((200..=260).contains(&built.width()), "{built:?}");
        // A price with itself is the one tick on the spacing that holds it.
        assert_eq!(
            TickRange::between(index, index).unwrap().width(),
            TICK_SPACING
        );

        // Prices are a geometric progression in the tick, so the geometric
        // mean of a band's ends is the price at its middle tick.
        let band = range(46_020, 46_080);
        let centre = band.geomean() / Price::at_tick(46_050).unwrap();
        assert!((centre - 1.0).abs() < 1e-12, "{centre}");
        assert!(matches!(band.geomean().tick().unwrap(), 46_049 | 46_050));
    }

    fn range(lower: i32, upper: i32) -> TickRange {
        TickRange::new(lower, upper).unwrap()
    }

    #[test]
    fn a_range_is_valid_by_construction() {
        for (lower, upper) in [
            (600, 600),
            (600, -600),
            (MIN_TICK - 1, 0),
            (0, MAX_TICK + 1),
        ] {
            assert!(
                matches!(
                    TickRange::new(lower, upper),
                    Err(ValidationError::InvalidTickRange { .. })
                ),
                "[{lower}, {upper}]"
            );
        }
        assert_eq!(range(MIN_TICK, MAX_TICK).width(), MAX_TICK - MIN_TICK);
    }

    #[test]
    fn contains_is_half_open_as_v4_activates_liquidity() {
        let range = range(100, 200);
        assert!(range.contains(100), "active from the lower bound");
        assert!(range.contains(199));
        assert!(!range.contains(200), "inactive at the upper bound");
        assert!(!range.contains(99));
    }

    #[test]
    fn a_range_knows_its_own_boundaries() {
        let range = range(100, 200);
        assert!(range.is_boundary(100) && range.is_boundary(200));
        assert!(!range.is_boundary(150), "inside is not on");
        assert!(!range.is_boundary(201));
        assert_eq!(range.width(), 100);
    }

    #[test]
    fn sqrt_bounds_are_the_tick_math_at_each_bound() {
        let range = range(-60, 60);
        assert_eq!(
            range.sqrt_bounds(),
            (
                get_sqrt_ratio_at_tick(-60).unwrap(),
                get_sqrt_ratio_at_tick(60).unwrap()
            )
        );
    }

    /// A range read back from JSON is checked like one built in code.
    #[test]
    fn serde_keeps_the_invariant() {
        let band = MakerBand::new(range(38_340, 38_430), LUnits::new(7));
        let json = serde_json::to_string(&band).unwrap();
        assert_eq!(
            json,
            r#"{"range":{"lower":38340,"upper":38430},"liquidity":7}"#
        );
        assert_eq!(serde_json::from_str::<MakerBand>(&json).unwrap(), band);
        assert!(
            serde_json::from_str::<TickRange>(r#"{"lower":10,"upper":10}"#).is_err(),
            "an empty range does not deserialise"
        );
    }
}
