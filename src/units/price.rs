//! The market's prices, and the square roots the pool stores.
//!
//! Both are Q96 fixed point on chain, and a price is the square of its own
//! root, so as primitives they are the same `U256` and swapping them
//! produces a number wrong by a squaring. The types keep them apart and
//! [`SqrtPrice::squared`] is the one way across.

use alloy::primitives::{U256, U512};

use crate::constants::{MAX_SQRT_PRICE_X96, MIN_SQRT_PRICE_X96, Q96, WAD};
use crate::errors::ValidationError;

use super::{BIGINT_1E6, F64_1E6, F64_WAD, Factor, MAX_SAFE_F64_INT, Share};

/// A price: USDC per unit of the market's token.
///
/// Held as the contract holds it, Q96 fixed point, reachable with
/// [`Self::x96`]. The pool price, the beacon's index, the EMAs of both and
/// the mark the contract values positions at are all this type.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct Price(U256);

/// The square root of a price, Q96 fixed point, which is what Uniswap
/// stores and what every liquidity formula is linear in.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct SqrtPrice(U256);

impl Price {
    /// From the contract's Q96 word.
    pub const fn from_x96(price_x96: U256) -> Self {
        Self(price_x96)
    }

    /// The Q96 word, for exact arithmetic.
    pub const fn x96(self) -> U256 {
        self.0
    }

    /// Whether the price is zero, which no market has: a zero here is a
    /// read that failed rather than a price.
    pub fn is_zero(self) -> bool {
        self.0.is_zero()
    }

    /// The price as a person reads it.
    ///
    /// Lossy, within [`Q96_PRECISION`](crate::constants::Q96_PRECISION).
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidPrice`] when the price is zero, and
    /// [`ValidationError::Overflow`] when it is past the range an `f64`
    /// represents exactly.
    pub fn to_f64(self) -> Result<f64, ValidationError> {
        if self.0.is_zero() {
            return Err(ValidationError::InvalidPrice {
                reason: "Q96 price value must be non-zero".into(),
            });
        }
        // `U256` multiplication wraps rather than panicking, and a wrapped
        // product can land small enough to pass the bound below and return a
        // plausible wrong price. The scaling is checked so that it cannot.
        let intermediate =
            self.0
                .checked_mul(BIGINT_1E6)
                .ok_or_else(|| ValidationError::Overflow {
                    context: "Q96 price exceeds safe f64 integer range after scaling".into(),
                })?
                / Q96;
        if intermediate > U256::from(MAX_SAFE_F64_INT) {
            return Err(ValidationError::Overflow {
                context: "Q96 price exceeds safe f64 integer range after scaling".into(),
            });
        }
        Ok(intermediate.as_limbs()[0] as f64 / F64_1E6)
    }
}

/// The price as a person reads it, for logs and reports. A price with no
/// reading ([`Price::to_f64`]), or one so small the reading is zero, shows
/// its Q96 word with an `x96` suffix instead.
impl std::fmt::Display for Price {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.to_f64() {
            Ok(price) if price > 0.0 => std::fmt::Display::fmt(&price, f),
            Ok(_) | Err(_) => write!(f, "{}x96", self.0),
        }
    }
}

/// The root's price as a person reads it; see [`Price`]'s `Display`.
impl std::fmt::Display for SqrtPrice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.price() {
            Ok(price) if price > 0.0 => std::fmt::Display::fmt(&price, f),
            Ok(_) | Err(_) => write!(f, "{}x96", self.0),
        }
    }
}

/// A price scaled by any [`Factor`]: a stop at half the mark, a landing
/// zone a few percent off the index. Exact in Q96, truncated toward zero;
/// a negative factor panics, since a price has no sign.
impl<F: Factor> std::ops::Mul<F> for Price {
    type Output = Self;
    fn mul(self, factor: F) -> Self {
        assert!(
            !factor.is_negative(),
            "a price cannot scale by a negative factor"
        );
        Self(factor.apply(self.0))
    }
}

impl std::ops::Mul<Price> for Share {
    type Output = Price;
    fn mul(self, price: Price) -> Price {
        price * self
    }
}

impl std::ops::Mul<Price> for f64 {
    type Output = Price;
    fn mul(self, price: Price) -> Price {
        price * self
    }
}

/// One price as a multiple of another: `mark / index` is the number a basis
/// is one less than. Exact in 256 bits, then one conversion to the `f64` a
/// strategy reasons in. Panics on a zero divisor, as the primitive does.
impl std::ops::Div for Price {
    type Output = f64;
    fn div(self, rhs: Self) -> f64 {
        assert!(!rhs.0.is_zero(), "a price ratio against a zero price");
        let wad = U512::from(self.0) * U512::from(WAD) / U512::from(rhs.0);
        u128::try_from(wad).expect("a ratio of two protocol prices fits WAD in u128") as f64
            / F64_WAD
    }
}

impl TryFrom<f64> for Price {
    type Error = ValidationError;

    /// A price a person wrote, in Q96.
    ///
    /// The two-step conversion keeps the whole `f64` mantissa: the price
    /// times 2^48 fits a `u128` for everything accepted, and the remaining
    /// factor is an exact shift.
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidPrice`] when the price is not a positive
    /// finite number, or is at least 2^80, where the first step would
    /// overflow.
    fn try_from(price: f64) -> Result<Self, Self::Error> {
        if !price.is_finite() || price <= 0.0 {
            return Err(ValidationError::InvalidPrice {
                reason: format!("price must be a positive finite number, got {price}"),
            });
        }
        const PRICE_LIMIT: f64 = (1u128 << 80) as f64;
        if price >= PRICE_LIMIT {
            return Err(ValidationError::InvalidPrice {
                reason: format!("price {price} exceeds the representable Q96 range (2^80)"),
            });
        }
        let hi = (price * (1u64 << 48) as f64) as u128;
        Ok(Self(U256::from(hi) << 48))
    }
}

impl SqrtPrice {
    /// From the contract's Q96 word.
    pub const fn from_x96(sqrt_price_x96: U256) -> Self {
        Self(sqrt_price_x96)
    }

    /// The Q96 word, for exact arithmetic.
    pub const fn x96(self) -> U256 {
        self.0
    }

    /// Whether the root is zero, which no initialised pool has.
    pub fn is_zero(self) -> bool {
        self.0.is_zero()
    }

    /// The price this is the root of, exactly.
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidPrice`] when the root is zero, and
    /// [`ValidationError::Overflow`] when squaring it leaves `U256`.
    pub fn squared(self) -> Result<Price, ValidationError> {
        if self.0.is_zero() {
            return Err(ValidationError::InvalidPrice {
                reason: "sqrtPriceX96 must be non-zero".into(),
            });
        }
        let squared = self
            .0
            .checked_mul(self.0)
            .ok_or_else(|| ValidationError::Overflow {
                context: "sqrtPriceX96² overflows U256".into(),
            })?;
        Ok(Price::from_x96(squared / Q96))
    }

    /// The root of `price`, where `price` is what a person wrote.
    ///
    /// Takes the root in `f64` and keeps a 6-decimal intermediate, so the
    /// result is approximate; the exact path from a tick is
    /// [`get_sqrt_ratio_at_tick`](crate::math::tick::get_sqrt_ratio_at_tick).
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidPrice`] when `price` is not a positive
    /// finite number, or is past 1e30.
    pub fn from_price(price: f64) -> Result<Self, ValidationError> {
        if !price.is_finite() || price <= 0.0 {
            return Err(ValidationError::InvalidPrice {
                reason: format!("price must be a positive finite number, got {price}"),
            });
        }
        if price > 1e30 {
            return Err(ValidationError::InvalidPrice {
                reason: format!("price {price} exceeds maximum (1e30)"),
            });
        }
        let scaled = price.sqrt() * F64_1E6;
        if scaled > MAX_SAFE_F64_INT as f64 {
            return Err(ValidationError::InvalidPrice {
                reason: format!("scaled sqrt(price) {scaled} exceeds safe f64 integer range"),
            });
        }
        // Through `u128` rather than straight to `U256`, so the float
        // narrowing is the platform-independent one.
        Ok(Self((U256::from(scaled as u128) * Q96) / BIGINT_1E6))
    }

    /// The price this is the root of, as a person reads it.
    ///
    /// # Errors
    ///
    /// As [`Self::squared`] and [`Price::to_f64`].
    pub fn price(self) -> Result<f64, ValidationError> {
        self.squared()?.to_f64()
    }
}

impl TryFrom<Price> for SqrtPrice {
    type Error = ValidationError;

    /// The root of a price, exact: the integer square root of the Q96
    /// word, the inverse of [`SqrtPrice::squared`]. Floored, so a price
    /// that was itself floored from a root comes back at most `2^96 / 2r`
    /// units under that root — nothing relative to the root, but enough
    /// that a tick boundary can resolve to the tick below, which is why a
    /// range built from prices aligns to the spacing rather than trusting
    /// the tick to the unit.
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidPrice`] when the root lies outside the
    /// protocol's `[MIN_SQRT_PRICE_X96, MAX_SQRT_PRICE_X96]`, which no
    /// pool can hold.
    fn try_from(price: Price) -> Result<Self, ValidationError> {
        let root = (U512::from(price.0) * U512::from(Q96)).root(2);
        if root < U512::from(MIN_SQRT_PRICE_X96) || root > U512::from(MAX_SQRT_PRICE_X96) {
            return Err(ValidationError::InvalidPrice {
                reason: format!("price's root {root} is outside the protocol's bounds"),
            });
        }
        // Inside the protocol's bounds, so inside the low four limbs.
        let limbs: [u64; 4] = root.as_limbs()[..4]
            .try_into()
            .expect("a U512 has at least four limbs");
        Ok(Self(U256::from_limbs(limbs)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A price displays as its human reading, with the formatter's
    /// precision, and a price with no reading shows its word.
    #[test]
    fn a_price_displays_as_a_person_reads_it() {
        let price = Price::try_from(1.5).unwrap();
        assert_eq!(format!("{price}"), "1.5");
        assert_eq!(format!("{price:.2}"), "1.50");
        // The root is floored, so its price reads a hair under.
        assert_eq!(
            format!("{:.4}", SqrtPrice::try_from(price).unwrap()),
            "1.5000"
        );
        assert_eq!(format!("{}", Price::from_x96(U256::ZERO)), "0x96");
        // The protocol's floor reads as zero, which is not a price a person
        // should be shown; the word is.
        let floor = Price::from_x96(Q96 / U256::from(1_000_000u64));
        assert_eq!(format!("{floor}"), format!("{}x96", floor.x96()));
        let root = SqrtPrice::from_x96(MIN_SQRT_PRICE_X96);
        assert_eq!(format!("{root}"), format!("{}x96", MIN_SQRT_PRICE_X96));
    }

    /// A price scales by any factor and divides into a plain number, which
    /// is what a stop and a basis are.
    #[test]
    fn a_price_scales_and_divides() {
        let mark = Price::try_from(100.0).unwrap();
        let index = Price::try_from(80.0).unwrap();
        assert_eq!(mark * 0.5, Price::try_from(50.0).unwrap());
        assert_eq!(0.5 * mark, Price::try_from(50.0).unwrap());
        assert_eq!(
            mark * Share::try_from(0.25).unwrap(),
            Price::try_from(25.0).unwrap()
        );
        assert_eq!(mark * 1.5, Price::try_from(150.0).unwrap());
        assert_eq!(mark / index, 1.25);
        assert_eq!(index / mark, 0.8);
        assert!(
            ((mark / index - 1.0) - 0.25).abs() < 1e-12,
            "the basis is one less"
        );
    }

    #[test]
    #[should_panic(expected = "negative factor")]
    fn a_price_has_no_sign_to_flip() {
        let _ = Price::try_from(1.0).unwrap() * -1.0;
    }

    /// The root of a price is the inverse of squaring it, exact to the
    /// floor, and refuses a price no pool can hold.
    #[test]
    fn a_root_is_the_inverse_of_squaring() {
        for root in [
            MIN_SQRT_PRICE_X96 * U256::from(2u8),
            Q96,
            Q96 * U256::from(7u8),
            MAX_SQRT_PRICE_X96 - U256::from(1u8),
        ] {
            let sqrt = SqrtPrice::from_x96(root);
            let back = SqrtPrice::try_from(sqrt.squared().unwrap()).unwrap();
            // The floor the square took costs at most 2^96 / 2r root
            // units, and the second floor one more.
            assert!(
                back.x96() <= root,
                "{root}: came back above, as {}",
                back.x96()
            );
            let lost = root - back.x96();
            assert!(
                lost <= U256::from(1u8) || lost * root <= Q96,
                "{root}: came back as {}",
                back.x96()
            );
        }
        assert!(SqrtPrice::try_from(Price::from_x96(U256::ZERO)).is_err());
        assert!(SqrtPrice::try_from(Price::from_x96(U256::MAX)).is_err());
    }
    use crate::constants::{MAX_SQRT_PRICE_X96, MIN_SQRT_PRICE_X96, Q96_PRECISION};

    /// Q96 encodes one as the scale itself, and the conversions are
    /// inverses within the documented bound. The table is what the scale
    /// means: half the word is half the price, a hundred times it is a
    /// hundred.
    #[test]
    fn a_price_round_trips_through_q96() {
        assert_eq!(Price::from_x96(Q96).to_f64().unwrap(), 1.0);
        let price = Price::try_from(1.5).unwrap();
        assert!((price.to_f64().unwrap() - 1.5).abs() < 1e-9);

        for (word, expected) in [
            (Q96 / U256::from(2u64), 0.5),
            (Q96 * U256::from(100u64), 100.0),
        ] {
            let read = Price::from_x96(word).to_f64().unwrap();
            assert!((read - expected).abs() < Q96_PRECISION, "{expected}");
        }
    }

    /// The `f64` door is exact only below 2^80: the conversion shifts a
    /// mantissa by 48 bits twice, so a price at or past the bound is refused
    /// rather than silently losing the low half.
    #[test]
    fn a_price_past_the_mantissa_bound_is_refused() {
        let bound = (1u128 << 80) as f64;
        for past in [bound, bound * 2.0, f64::MAX] {
            assert!(Price::try_from(past).is_err(), "{past}");
        }
        assert!(Price::try_from(bound * (1.0 - f64::EPSILON)).is_ok());
    }

    /// A price is the square of its root, and the root of one is the scale.
    /// The table is the squaring: twice the word is four times the price,
    /// half of it a quarter.
    #[test]
    fn a_root_squares_back_to_its_price() {
        assert_eq!(
            SqrtPrice::from_x96(Q96).squared().unwrap(),
            Price::from_x96(Q96)
        );
        assert!((SqrtPrice::from_x96(Q96).price().unwrap() - 1.0).abs() < Q96_PRECISION);

        let root = SqrtPrice::from_price(4.0).unwrap();
        assert!((root.price().unwrap() - 4.0).abs() < 1e-5);

        for (word, expected) in [
            (Q96 * U256::from(2u64), 4.0),
            (Q96 / U256::from(2u64), 0.25),
        ] {
            let read = SqrtPrice::from_x96(word).price().unwrap();
            assert!((read - expected).abs() < Q96_PRECISION, "{expected}");
        }

        // The protocol's widest root is a price of 1e6, its largest
        // starting price; squaring a word near `U256::MAX` leaves the type
        // rather than wrapping.
        let widest = SqrtPrice::from_x96(MAX_SQRT_PRICE_X96).price().unwrap();
        assert!((widest - 1e6).abs() < Q96_PRECISION, "{widest}");
        assert!(
            SqrtPrice::from_x96(U256::MAX / U256::from(2u64))
                .squared()
                .is_err()
        );
    }

    /// A price survives the trip out to a root and back. The intermediate is
    /// 6-decimal, so the error is relative and widens away from one — which
    /// is why the protocol's two ends are in the table and the tolerance
    /// near one is ten times tighter.
    #[test]
    fn a_price_survives_the_trip_through_its_root() {
        // The table starts at 0.01, and that bound is the point: `to_f64`'s
        // resolution is 1e-6 *absolute*, so a relative tolerance only holds
        // well above it — 1e-5 comes back as 9e-6, which is 10% out, and
        // 1e-6 comes back as nothing. SDK #152 tracks it; the floor is
        // pinned by `the_f64_view_reads_the_protocols_floor_as_zero`.
        for price in [0.01, 0.1, 0.5, 1.0, 2.0, 10.0, 100.0, 500.0, 1e6] {
            let root = SqrtPrice::from_price(price).unwrap();
            let back = root.price().unwrap();
            let relative = (back - price).abs() / price;
            assert!(relative < 0.001, "{price} came back as {back}");
        }

        let near_one = SqrtPrice::from_price(1.05).unwrap().price().unwrap();
        assert!((near_one - 1.05).abs() / 1.05 < 0.0001, "{near_one}");
    }

    /// A word whose scaling leaves `U256` is refused rather than wrapped.
    ///
    /// The value below is chosen so that `word × 1e6` wraps to exactly
    /// `Q96`: an unchecked multiply answers `Ok(1e-6)` for it, which is a
    /// plausible price and the wrong one. No pool holds a word this large,
    /// but a fallible conversion must fail rather than invent a number.
    #[test]
    fn a_price_whose_scaling_wraps_is_refused() {
        let wraps_to_one = U256::from_str_radix(
            "561475840711746231608895706307127665180506155643691174255492339118708359168",
            10,
        )
        .unwrap();
        assert!(matches!(
            Price::from_x96(wraps_to_one).to_f64(),
            Err(ValidationError::Overflow { .. })
        ));
        assert!(matches!(
            Price::from_x96(U256::MAX).to_f64(),
            Err(ValidationError::Overflow { .. })
        ));
    }

    /// Zero is not a price, on either type, and neither is a value a
    /// person could not have meant.
    #[test]
    fn zero_and_the_impossible_are_refused() {
        assert!(Price::from_x96(U256::ZERO).to_f64().is_err());
        assert!(SqrtPrice::from_x96(U256::ZERO).squared().is_err());
        for price in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(Price::try_from(price).is_err(), "{price}");
            assert!(SqrtPrice::from_price(price).is_err(), "{price}");
        }
    }

    /// The `f64` view bottoms out *at* the protocol's minimum price rather
    /// than below it, and returns zero rather than failing.
    ///
    /// `to_f64` scales by a million and divides by the scale in integers, so
    /// a word at `Q96 / 1e6` — which is what `MIN_SQRT_PRICE_X96` squares to,
    /// the lowest price a market can hold — leaves nothing in the
    /// intermediate and reads as `0.0`. Note the asymmetry this test exists
    /// to make visible: the same function *refuses* a zero input word, so it
    /// will not read a zero price but will happily return one. A consumer
    /// testing `price > 0.0` concludes a market at its floor has no price.
    /// Tracked as SDK #152; pinned here so a fix has to change a test.
    #[test]
    fn the_f64_view_reads_the_protocols_floor_as_zero() {
        assert_eq!(
            SqrtPrice::from_x96(MIN_SQRT_PRICE_X96).price().unwrap(),
            0.0
        );
        assert_eq!(
            Price::from_x96(Q96 / U256::from(1_000_000u64))
                .to_f64()
                .unwrap(),
            0.0
        );
        // And the input it does refuse, for the contrast.
        assert!(Price::from_x96(U256::ZERO).to_f64().is_err());
    }

    /// Prices compare, which is what the swap bounds need of them.
    #[test]
    fn prices_order() {
        assert!(Price::try_from(1.0).unwrap() < Price::try_from(2.0).unwrap());
        assert!(SqrtPrice::from_price(1.0).unwrap() < SqrtPrice::from_price(2.0).unwrap());
    }

    /// The wire form is the bare number.
    #[test]
    fn the_wire_form_is_the_number_alone() {
        let json = serde_json::to_string(&Price::from_x96(Q96)).unwrap();
        assert_eq!(serde_json::from_str::<Price>(&json).unwrap().x96(), Q96);
    }
}
