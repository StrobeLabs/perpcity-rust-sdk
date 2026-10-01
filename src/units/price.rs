//! The market's prices, and the square roots the pool stores.
//!
//! Both are Q96 fixed point on chain, and a price is the square of its own
//! root, so as primitives they are the same `U256` and swapping them
//! produces a number wrong by a squaring. The types keep them apart and
//! [`SqrtPrice::squared`] is the one way across.

use alloy::primitives::U256;

use crate::constants::Q96;
use crate::errors::ValidationError;

use super::{BIGINT_1E6, F64_1E6, MAX_SAFE_F64_INT};

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
        let intermediate = (self.0 * BIGINT_1E6) / Q96;
        if intermediate > U256::from(MAX_SAFE_F64_INT) {
            return Err(ValidationError::Overflow {
                context: "Q96 price exceeds safe f64 integer range after scaling".into(),
            });
        }
        Ok(intermediate.as_limbs()[0] as f64 / F64_1E6)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::Q96_PRECISION;

    /// Q96 encodes one as the scale itself, and the conversions are
    /// inverses within the documented bound.
    #[test]
    fn a_price_round_trips_through_q96() {
        assert_eq!(Price::from_x96(Q96).to_f64().unwrap(), 1.0);
        let price = Price::try_from(1.5).unwrap();
        assert!((price.to_f64().unwrap() - 1.5).abs() < 1e-9);
    }

    /// A price is the square of its root, and the root of one is the scale.
    #[test]
    fn a_root_squares_back_to_its_price() {
        assert_eq!(
            SqrtPrice::from_x96(Q96).squared().unwrap(),
            Price::from_x96(Q96)
        );
        assert!((SqrtPrice::from_x96(Q96).price().unwrap() - 1.0).abs() < Q96_PRECISION);

        let root = SqrtPrice::from_price(4.0).unwrap();
        assert!((root.price().unwrap() - 4.0).abs() < 1e-5);
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
