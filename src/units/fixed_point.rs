//! Solidity-compatible fixed-point primitives.
//!
//! [`mul_div`] mirrors the contracts' `FullMath.mulDiv`: the product is
//! computed in 512 bits so `a × b / d` cannot overflow before the division.
//! [`s_full_mul_div`] mirrors `SignedFixedPointMathLib.sFullMulDiv`,
//! including its round-toward-positive-infinity step that only increments
//! non-negative results. [`exp_wad`] is Solady's `expWad`, the rational
//! approximation and integer rounding the contracts use rather than a
//! floating-point `exp`.

use alloy::primitives::{I256, U256, U512, uint};

use crate::errors::ValidationError;

/// Solady's `expWad` power-of-two reassembly scale.
const EXP_SCALE: U256 = uint!(3822833074963236453042738258902158003155416615667_U256);

fn i(value: i128) -> I256 {
    I256::try_from(value).expect("i128 fits I256")
}

/// Solady's `expWad`: `e^x` for a WAD-scaled `x`, exact to the contract.
///
/// # Errors
///
/// [`ValidationError::Overflow`] where the contract reverts, `x ≥ 135.3`
/// WAD.
pub(crate) fn exp_wad(mut x: I256) -> Result<U256, ValidationError> {
    if x <= i(-41_446_531_673_892_822_313) {
        return Ok(U256::ZERO);
    }
    if x >= i(135_305_999_368_893_231_589) {
        return Err(ValidationError::Overflow {
            context: "expWad overflow".into(),
        });
    }

    x = x.wrapping_shl(78) / i(5i128.pow(18));
    let log2 = i(54_916_777_467_707_473_351_141_471_128);
    let k: I256 = (x
        .wrapping_shl(96)
        .wrapping_div(log2)
        .wrapping_add(I256::ONE.wrapping_shl(95)))
    .asr(96);
    x = x.wrapping_sub(k.wrapping_mul(log2));

    let mut y = x.wrapping_add(i(1_346_386_616_545_796_478_920_950_773_328));
    y = y
        .wrapping_mul(x)
        .asr(96)
        .wrapping_add(i(57_155_421_227_552_351_082_224_309_758_442));
    let mut p = y
        .wrapping_add(x)
        .wrapping_sub(i(94_201_549_194_550_492_254_356_042_504_812));
    p = p
        .wrapping_mul(y)
        .asr(96)
        .wrapping_add(i(28_719_021_644_029_726_153_956_944_680_412_240));
    p = p
        .wrapping_mul(x)
        .wrapping_add(i(4_385_272_521_454_847_904_659_076_985_693_276).wrapping_shl(96));

    let mut q = x.wrapping_sub(i(2_855_989_394_907_223_263_936_484_059_900));
    q = q
        .wrapping_mul(x)
        .asr(96)
        .wrapping_add(i(50_020_603_652_535_783_019_961_831_881_945));
    q = q
        .wrapping_mul(x)
        .asr(96)
        .wrapping_sub(i(533_845_033_583_426_703_283_633_433_725_380));
    q = q
        .wrapping_mul(x)
        .asr(96)
        .wrapping_add(i(3_604_857_256_930_695_427_073_651_918_091_429));
    q = q
        .wrapping_mul(x)
        .asr(96)
        .wrapping_sub(i(14_423_608_567_350_463_180_887_372_962_807_573));
    q = q
        .wrapping_mul(x)
        .asr(96)
        .wrapping_add(i(26_449_188_498_355_588_339_934_803_723_976_023));

    let r = p.wrapping_div(q);
    let r_u = U256::try_from(r).map_err(|_| ValidationError::Overflow {
        context: "negative expWad approximation".into(),
    })?;
    let k_i128 = i128::try_from(k).map_err(|_| ValidationError::Overflow {
        context: "expWad exponent".into(),
    })?;
    let shift = usize::try_from(195 - k_i128).map_err(|_| ValidationError::Overflow {
        context: "expWad shift".into(),
    })?;
    Ok(r_u.wrapping_mul(EXP_SCALE) >> shift)
}

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
    let q = match rounding {
        Rounding::Up => product.div_ceil(divisor),
        Rounding::TowardZero => product / divisor,
    };
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

    const WAD: i128 = 1_000_000_000_000_000_000;

    #[test]
    fn exp_matches_solady_vectors() {
        assert_eq!(exp_wad(i(0)).unwrap(), U256::from(WAD as u128));
        assert_eq!(
            exp_wad(i(-WAD)).unwrap(),
            U256::from(367_879_441_171_442_321u128)
        );
        assert_eq!(
            exp_wad(i(-3 * WAD)).unwrap(),
            U256::from(49_787_068_367_863_942u128)
        );
    }

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
