//! Solidity-compatible full-precision multiply-divide primitives.
//!
//! [`mul_div`] mirrors the contracts' `FullMath.mulDiv`: the product is
//! computed in 512 bits so `a × b / d` cannot overflow before the division.
//! [`s_full_mul_div`] mirrors `SignedFixedPointMathLib.sFullMulDiv`,
//! including its round-toward-positive-infinity step that only increments
//! non-negative results.

use alloy::primitives::{I256, U256, U512};

use crate::errors::ValidationError;

/// How an inexact division resolves its remainder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rounding {
    /// Truncate toward zero (Solidity's default division).
    TowardZero,
    /// Round away from zero when the division has a remainder.
    Up,
}

/// `a × b / d` with a 512-bit intermediate product.
///
/// # Errors
///
/// Returns [`ValidationError::Overflow`] when `d` is zero or the quotient
/// exceeds `U256`.
pub(crate) fn mul_div(
    a: U256,
    b: U256,
    d: U256,
    rounding: Rounding,
) -> Result<U256, ValidationError> {
    if d.is_zero() {
        return Err(ValidationError::Overflow {
            context: "division by zero".into(),
        });
    }
    let product: U512 = a.widening_mul(b);
    let divisor = U512::from(d);
    let mut q = product / divisor;
    if rounding == Rounding::Up && product % divisor != U512::ZERO {
        q += U512::ONE;
    }
    u512_to_u256(q)
}

/// The contracts' `sFullMulDiv`: signed mul-div with the magnitude truncated
/// toward zero. Under [`Rounding::Up`] a result with a remainder is
/// incremented by one **only when it is not negative** — rounding toward
/// positive infinity increments only non-exact non-negative results, while
/// negative results stay truncated toward zero. This mirrors the deployed
/// `SignedFixedPointMathLib.sFullMulDiv` (`perpcity-contracts@4bbe554f`):
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
    rounding: Rounding,
) -> Result<I256, ValidationError> {
    let negative = a.is_negative() != b.is_negative();
    // The contract's "+1 on a remainder, non-negative results only" is
    // exactly `mulDiv`'s own round-up applied to the magnitude. A zero
    // operand makes `negative` irrelevant: the magnitude is exactly zero
    // either way.
    let magnitude_rounding = if rounding == Rounding::Up && !negative {
        Rounding::Up
    } else {
        Rounding::TowardZero
    };
    let magnitude = mul_div(a.unsigned_abs(), b.unsigned_abs(), d, magnitude_rounding)?;
    let magnitude = I256::try_from(magnitude).map_err(|_| ValidationError::Overflow {
        context: "signed mul-div magnitude exceeds I256".into(),
    })?;
    Ok(if negative { -magnitude } else { magnitude })
}

/// Reinterpret an unsigned 256-bit value as signed, erroring when the
/// value exceeds `I256::MAX`.
pub(crate) fn to_i256(v: U256, context: &'static str) -> Result<I256, ValidationError> {
    I256::try_from(v).map_err(|_| ValidationError::Overflow {
        context: context.into(),
    })
}

// Chain-derived values must never wrap silently: alloy's `Signed` only
// debug-asserts on overflow and ruint's `Sub` wraps in release, so every
// add/sub on snapshot inputs goes through these checked helpers. An `Err`
// means corrupt or mutually inconsistent inputs (e.g. a position checkpoint
// ahead of the market cumulative), not a value to propagate.

/// Checked signed addition.
pub(crate) fn add_i(a: I256, b: I256, context: &'static str) -> Result<I256, ValidationError> {
    a.checked_add(b).ok_or(ValidationError::Overflow {
        context: context.into(),
    })
}

/// Checked signed subtraction.
pub(crate) fn sub_i(a: I256, b: I256, context: &'static str) -> Result<I256, ValidationError> {
    a.checked_sub(b).ok_or(ValidationError::Overflow {
        context: context.into(),
    })
}

/// Checked unsigned addition.
pub(crate) fn add_u(a: U256, b: U256, context: &'static str) -> Result<U256, ValidationError> {
    a.checked_add(b).ok_or(ValidationError::Overflow {
        context: context.into(),
    })
}

/// Checked unsigned subtraction.
pub(crate) fn sub_u(a: U256, b: U256, context: &'static str) -> Result<U256, ValidationError> {
    a.checked_sub(b).ok_or(ValidationError::Overflow {
        context: context.into(),
    })
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
        assert_eq!(smd(big(7), big(10), Rounding::TowardZero), big(0));
        assert_eq!(smd(big(7), big(10), Rounding::Up), big(1));
        assert_eq!(smd(big(-7), big(10), Rounding::TowardZero), big(0));
        // The contract's roundUp guards on `!negative`: a negative non-exact
        // result stays truncated toward zero instead of gaining +1.
        assert_eq!(smd(big(-7), big(10), Rounding::Up), big(0));
        assert_eq!(smd(big(-70), big(10), Rounding::TowardZero), big(-7));
        assert_eq!(smd(big(-70), big(10), Rounding::Up), big(-7));
        assert_eq!(smd(big(-75), big(10), Rounding::Up), big(-7));
        assert_eq!(smd(big(75), big(10), Rounding::Up), big(8));
    }

    #[test]
    fn division_by_zero_is_an_error() {
        assert!(mul_div(U256::ONE, U256::ONE, U256::ZERO, Rounding::TowardZero).is_err());
        assert!(s_full_mul_div(I256::ONE, I256::ONE, U256::ZERO, Rounding::Up).is_err());
    }
}
