//! Solidity-compatible full-precision multiply-divide primitives.
//!
//! [`mul_div`] mirrors the contracts' `FullMath.mulDiv`: the product is
//! computed in 512 bits so `a × b / d` cannot overflow before the division.
//! [`s_full_mul_div`] mirrors `SignedFixedPointMathLib.sFullMulDiv`,
//! including its round-toward-positive-infinity step that only increments
//! non-negative results.

use alloy::primitives::{I256, U256, U512};

use crate::errors::ValidationError;

/// `a × b / d` with a 512-bit intermediate product, rounding up when
/// `round_up` is set and the division has a remainder.
///
/// # Errors
///
/// Returns [`ValidationError::Overflow`] when `d` is zero or the quotient
/// exceeds `U256`.
pub(crate) fn mul_div(a: U256, b: U256, d: U256, round_up: bool) -> Result<U256, ValidationError> {
    if d.is_zero() {
        return Err(ValidationError::Overflow {
            context: "division by zero".into(),
        });
    }
    let product: U512 = a.widening_mul(b);
    let divisor = U512::from(d);
    let mut q = product / divisor;
    if round_up && product % divisor != U512::ZERO {
        q += U512::ONE;
    }
    u512_to_u256(q)
}

/// The contracts' `sFullMulDiv`: signed mul-div with the magnitude truncated
/// toward zero. When `round_up` is set and the division has a remainder, the
/// result is incremented by one **only when it is not negative** — rounding
/// toward positive infinity increments only non-exact non-negative results,
/// while negative results stay truncated toward zero. This mirrors the
/// deployed `SignedFixedPointMathLib.sFullMulDiv`
/// (`perpcity-contracts@4bbe554f`):
///
/// ```solidity
/// result = negative ? -absResult : absResult;
/// // Rounding toward positive infinity increments only non-exact
/// // positive results.
/// if (roundUp && !negative) {
///     bool hasRemainder = mulmod(unsignedA, unsignedB, denominator) != 0;
///     result += SafeCastLib.toInt256(hasRemainder ? 1 : 0);
/// }
/// ```
///
/// # Errors
///
/// Returns [`ValidationError::Overflow`] when `d` is zero or the magnitude
/// exceeds `I256`.
pub(crate) fn s_full_mul_div(
    a: I256,
    b: I256,
    d: U256,
    round_up: bool,
) -> Result<I256, ValidationError> {
    let (ua, ub) = (a.unsigned_abs(), b.unsigned_abs());
    let negative = (a.is_negative() && b > I256::ZERO) || (a > I256::ZERO && b.is_negative());
    let magnitude =
        I256::try_from(mul_div(ua, ub, d, false)?).map_err(|_| ValidationError::Overflow {
            context: "signed mul-div magnitude exceeds I256".into(),
        })?;
    let mut result = if negative { -magnitude } else { magnitude };
    if round_up && !negative && ua.widening_mul(ub) % U512::from(d) != U512::ZERO {
        result += I256::ONE;
    }
    Ok(result)
}

/// Narrow a 512-bit value to `U256`, erroring instead of truncating.
pub(crate) fn u512_to_u256(value: U512) -> Result<U256, ValidationError> {
    if value > U512::from(U256::MAX) {
        return Err(ValidationError::Overflow {
            context: "U512 to U256".into(),
        });
    }
    Ok(value.to::<U256>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s_full_mul_div_matches_contract_semantics() {
        let q = U256::from(100u8);
        let big = |v: i64| I256::try_from(v).unwrap();
        let smd = |a, b, r| s_full_mul_div(a, b, q, r).unwrap();
        assert_eq!(smd(big(7), big(10), false), big(0));
        assert_eq!(smd(big(7), big(10), true), big(1));
        assert_eq!(smd(big(-7), big(10), false), big(0));
        // The contract's roundUp guards on `!negative`: a negative non-exact
        // result stays truncated toward zero instead of gaining +1.
        assert_eq!(smd(big(-7), big(10), true), big(0));
        assert_eq!(smd(big(-70), big(10), false), big(-7));
        assert_eq!(smd(big(-70), big(10), true), big(-7));
        assert_eq!(smd(big(-75), big(10), true), big(-7));
        assert_eq!(smd(big(75), big(10), true), big(8));
    }

    #[test]
    fn division_by_zero_is_an_error() {
        assert!(mul_div(U256::ONE, U256::ONE, U256::ZERO, false).is_err());
        assert!(s_full_mul_div(I256::ONE, I256::ONE, U256::ZERO, true).is_err());
    }
}
