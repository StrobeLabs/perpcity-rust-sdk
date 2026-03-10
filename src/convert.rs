//! Conversions between client-facing types (`f64`, human-readable) and
//! on-chain types (6-decimal integers, `U256`, `sqrtPriceX96`).
//!
//! All conversion functions validate inputs and return [`Result`] on
//! failure. The math mirrors `perpcity-zig-sdk/src/conversions.zig` but
//! uses Alloy's `U256` instead of Zig's `u256`.
//!
//! # Precision model
//!
//! USDC has 6 decimals, so `1.0 USDC` = `1_000_000` on-chain. The
//! [`scale_to_6dec`] / [`scale_from_6dec`] pair handles this scaling.
//!
//! Uniswap V4 prices are stored as `sqrtPriceX96 = sqrt(price) × 2^96`.
//! The [`price_to_sqrt_price_x96`] / [`sqrt_price_x96_to_price`] pair
//! handles this encoding, using a 6-decimal intermediate for precision.

use alloy::primitives::U256;

use crate::constants::Q96;
use crate::errors::{PerpCityError, Result};

// ── Module-level constants ─────────────────────────────────────────────

/// 10^6 as f64, for floating-point scaling.
const F64_1E6: f64 = 1_000_000.0;

/// 10^6 as U256, for big-integer scaling.
const BIGINT_1E6: U256 = U256::from_limbs([1_000_000, 0, 0, 0]);

/// Maximum integer exactly representable as f64 (2^53).
/// Values beyond this lose precision in float ↔ int conversion.
const MAX_SAFE_F64_INT: u64 = 1_u64 << 53; // 9_007_199_254_740_992

// ── Scaling: f64 ↔ 6-decimal integers ──────────────────────────────────

/// Scale a human-readable amount to its 6-decimal on-chain representation.
///
/// Supports negative values (for `marginDelta`, `usdDelta`, etc.).
/// Uses `floor` to match Solidity's truncation semantics.
///
/// # Errors
///
/// Returns [`PerpCityError::Overflow`] if `|amount|` exceeds the safe
/// f64 integer range (2^53).
///
/// # Examples
///
/// ```
/// # use perpcity_rust_sdk::convert::scale_to_6dec;
/// assert_eq!(scale_to_6dec(1.5).unwrap(), 1_500_000);
/// assert_eq!(scale_to_6dec(-2.5).unwrap(), -2_500_000);
/// ```
pub fn scale_to_6dec(amount: f64) -> Result<i128> {
    if amount.is_nan() || amount.is_infinite() {
        return Err(PerpCityError::Overflow {
            context: format!("amount {amount} is not finite"),
        });
    }
    if amount.abs() > MAX_SAFE_F64_INT as f64 {
        return Err(PerpCityError::Overflow {
            context: format!("amount {amount} exceeds safe f64 integer range (2^53)"),
        });
    }
    Ok((amount * F64_1E6).floor() as i128)
}

/// Convert a 6-decimal on-chain value back to human-readable f64.
///
/// Accepts `i128` for symmetry with [`scale_to_6dec`], which returns `i128`
/// to support signed on-chain values (`int256` marginDelta, usdDelta, etc.).
///
/// This is a simple division — it cannot fail.
///
/// # Examples
///
/// ```
/// # use perpcity_rust_sdk::convert::scale_from_6dec;
/// assert_eq!(scale_from_6dec(1_500_000), 1.5);
/// assert_eq!(scale_from_6dec(-2_000_000), -2.0);
/// ```
pub fn scale_from_6dec(value: i128) -> f64 {
    value as f64 / F64_1E6
}

// ── Leverage ↔ margin ratio ────────────────────────────────────────────

/// Convert leverage (e.g. `10.0` for 10×) to an on-chain margin ratio
/// scaled by 1e6.
///
/// On-chain: `marginRatio = 1_000_000 / leverage`.
/// - 1× leverage → margin ratio `1_000_000` (100%)
/// - 10× leverage → margin ratio `100_000` (10%)
/// - 100× leverage → margin ratio `10_000` (1%)
///
/// # Errors
///
/// Returns [`PerpCityError::InvalidLeverage`] if leverage is zero,
/// negative, NaN, or produces an out-of-range margin ratio.
///
/// # Examples
///
/// ```
/// # use perpcity_rust_sdk::convert::leverage_to_margin_ratio;
/// assert_eq!(leverage_to_margin_ratio(10.0).unwrap(), 100_000);
/// assert_eq!(leverage_to_margin_ratio(1.0).unwrap(), 1_000_000);
/// assert_eq!(leverage_to_margin_ratio(100.0).unwrap(), 10_000);
/// ```
pub fn leverage_to_margin_ratio(leverage: f64) -> Result<u32> {
    if leverage.is_nan() || leverage.is_infinite() || leverage <= 0.0 {
        return Err(PerpCityError::InvalidLeverage {
            reason: format!("leverage must be a positive finite number, got {leverage}"),
        });
    }
    let ratio = (F64_1E6 / leverage).round();
    if ratio < 1.0 || ratio > u32::MAX as f64 {
        return Err(PerpCityError::InvalidLeverage {
            reason: format!("leverage {leverage} produces out-of-range margin ratio {ratio}"),
        });
    }
    Ok(ratio as u32)
}

/// Convert an on-chain margin ratio (scaled by 1e6) to leverage.
///
/// On-chain: `leverage = 1_000_000 / marginRatio`.
///
/// # Errors
///
/// Returns [`PerpCityError::InvalidMarginRatio`] if `margin_ratio` is zero.
///
/// # Examples
///
/// ```
/// # use perpcity_rust_sdk::convert::margin_ratio_to_leverage;
/// let lev = margin_ratio_to_leverage(100_000).unwrap();
/// assert!((lev - 10.0).abs() < 0.0001);
/// ```
pub fn margin_ratio_to_leverage(margin_ratio: u32) -> Result<f64> {
    if margin_ratio == 0 {
        return Err(PerpCityError::InvalidMarginRatio {
            value: 0,
            min: 1,
            max: u32::MAX,
        });
    }
    Ok(F64_1E6 / margin_ratio as f64)
}

// ── Price ↔ sqrtPriceX96 ──────────────────────────────────────────────

/// Convert a human-readable price to `sqrtPriceX96` (Uniswap V4 format).
///
/// Formula (using 6-decimal intermediate for precision):
/// 1. `sqrt_price = sqrt(price)`
/// 2. `scaled = floor(sqrt_price × 1e6)` → `U256`
/// 3. `result = scaled × 2^96 / 1e6`
///
/// # Errors
///
/// Returns [`PerpCityError::InvalidPrice`] if `price` is zero, negative,
/// or too large (> 1e30).
///
/// # Examples
///
/// ```
/// # use perpcity_rust_sdk::convert::price_to_sqrt_price_x96;
/// # use perpcity_rust_sdk::constants::Q96;
/// # use alloy::primitives::U256;
/// let result = price_to_sqrt_price_x96(1.0).unwrap();
/// // For price=1.0, sqrtPriceX96 ≈ Q96
/// let diff = result.abs_diff(Q96);
/// assert!(diff < Q96 / U256::from(1_000_000));
/// ```
pub fn price_to_sqrt_price_x96(price: f64) -> Result<U256> {
    if price.is_nan() || price.is_infinite() || price <= 0.0 {
        return Err(PerpCityError::InvalidPrice {
            reason: format!("price must be a positive finite number, got {price}"),
        });
    }
    if price > 1e30 {
        return Err(PerpCityError::InvalidPrice {
            reason: format!("price {price} exceeds maximum (1e30)"),
        });
    }

    let sqrt_price = price.sqrt();
    let scaled = sqrt_price * F64_1E6;

    if scaled > MAX_SAFE_F64_INT as f64 {
        return Err(PerpCityError::InvalidPrice {
            reason: format!("scaled sqrt(price) {scaled} exceeds safe f64 integer range"),
        });
    }

    // Cast via u128 to avoid platform-specific float→bigint issues.
    let scaled_int = U256::from(scaled as u128);
    Ok((scaled_int * Q96) / BIGINT_1E6)
}

/// Convert a `sqrtPriceX96` value back to a human-readable price.
///
/// Formula:
/// 1. `price_x96 = sqrtPriceX96² / 2^96`
/// 2. `intermediate = price_x96 × 1e6 / 2^96`
/// 3. `price = intermediate / 1e6`
///
/// # Errors
///
/// Returns [`PerpCityError::InvalidPrice`] if `sqrt_price_x96` is zero,
/// or [`PerpCityError::Overflow`] if the squared value overflows `U256`
/// or the result exceeds safe f64 range.
///
/// # Examples
///
/// ```
/// # use perpcity_rust_sdk::convert::sqrt_price_x96_to_price;
/// # use perpcity_rust_sdk::constants::Q96;
/// let price = sqrt_price_x96_to_price(Q96).unwrap();
/// assert!((price - 1.0).abs() < 0.000001);
/// ```
pub fn sqrt_price_x96_to_price(sqrt_price_x96: U256) -> Result<f64> {
    if sqrt_price_x96.is_zero() {
        return Err(PerpCityError::InvalidPrice {
            reason: "sqrtPriceX96 must be non-zero".into(),
        });
    }

    let squared =
        sqrt_price_x96
            .checked_mul(sqrt_price_x96)
            .ok_or_else(|| PerpCityError::Overflow {
                context: "sqrtPriceX96² overflows U256".into(),
            })?;

    let price_x96 = squared / Q96;

    // scaleFromX96: (value × 1e6 / Q96) → integer, then ÷ 1e6
    let intermediate = (price_x96 * BIGINT_1E6) / Q96;

    if intermediate > U256::from(MAX_SAFE_F64_INT) {
        return Err(PerpCityError::Overflow {
            context: "price exceeds safe f64 integer range after scaling".into(),
        });
    }

    // Safe: we verified intermediate ≤ MAX_SAFE_F64_INT < u64::MAX.
    let int_val = intermediate.as_limbs()[0];
    Ok(int_val as f64 / F64_1E6)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{MAX_SQRT_PRICE_X96, MIN_SQRT_PRICE_X96};

    // ── scale_to_6dec ──────────────────────────────────────────────

    #[test]
    fn scale_to_6dec_positive() {
        assert_eq!(scale_to_6dec(1.5).unwrap(), 1_500_000);
    }

    #[test]
    fn scale_to_6dec_zero() {
        assert_eq!(scale_to_6dec(0.0).unwrap(), 0);
    }

    #[test]
    fn scale_to_6dec_negative() {
        assert_eq!(scale_to_6dec(-2.5).unwrap(), -2_500_000);
    }

    #[test]
    fn scale_to_6dec_small_fractional() {
        // Smallest representable unit: 0.000001 = 1
        assert_eq!(scale_to_6dec(0.000001).unwrap(), 1);
    }

    #[test]
    fn scale_to_6dec_truncates_below_6_decimals() {
        // 1.1234567 * 1e6 = 1123456.7, floor = 1123456
        assert_eq!(scale_to_6dec(1.1234567).unwrap(), 1_123_456);
    }

    #[test]
    fn scale_to_6dec_negative_truncation_floors_toward_negative_infinity() {
        // -1.1234567 * 1e6 = -1123456.7, floor = -1123457
        assert_eq!(scale_to_6dec(-1.1234567).unwrap(), -1_123_457);
    }

    #[test]
    fn scale_to_6dec_large_valid_amount() {
        // 1 billion USDC — should work
        let result = scale_to_6dec(1_000_000_000.0).unwrap();
        assert_eq!(result, 1_000_000_000_000_000);
    }

    #[test]
    fn scale_to_6dec_overflow_positive() {
        // 1e16 > 2^53 ≈ 9.007e15, clearly exceeds safe f64 integer range
        assert!(scale_to_6dec(1e16).is_err());
    }

    #[test]
    fn scale_to_6dec_overflow_negative() {
        assert!(scale_to_6dec(-1e16).is_err());
    }

    #[test]
    fn scale_to_6dec_nan() {
        assert!(scale_to_6dec(f64::NAN).is_err());
    }

    #[test]
    fn scale_to_6dec_infinity() {
        assert!(scale_to_6dec(f64::INFINITY).is_err());
        assert!(scale_to_6dec(f64::NEG_INFINITY).is_err());
    }

    // ── scale_from_6dec ────────────────────────────────────────────

    #[test]
    fn scale_from_6dec_positive() {
        assert_eq!(scale_from_6dec(1_500_000), 1.5);
    }

    #[test]
    fn scale_from_6dec_zero() {
        assert_eq!(scale_from_6dec(0), 0.0);
    }

    #[test]
    fn scale_from_6dec_negative() {
        assert_eq!(scale_from_6dec(-2_000_000), -2.0);
    }

    #[test]
    fn scale_from_6dec_one_unit() {
        assert_eq!(scale_from_6dec(1), 0.000001);
    }

    #[test]
    fn scale_from_6dec_five_usdc() {
        // 5 USDC = 5_000_000 on-chain
        assert_eq!(scale_from_6dec(5_000_000), 5.0);
    }

    // ── scale roundtrip ────────────────────────────────────────────

    #[test]
    fn scale_6dec_roundtrip_exact() {
        // Values with at most 6 decimal places roundtrip exactly.
        for &amount in &[0.0, 1.0, 0.5, 100.123456, -50.0, 999_999.999999] {
            let scaled = scale_to_6dec(amount).unwrap();
            let recovered = scale_from_6dec(scaled);
            assert!(
                (recovered - amount).abs() < 1e-6,
                "roundtrip failed for {amount}: got {recovered}"
            );
        }
    }

    // ── leverage_to_margin_ratio ───────────────────────────────────

    #[test]
    fn leverage_1x() {
        assert_eq!(leverage_to_margin_ratio(1.0).unwrap(), 1_000_000);
    }

    #[test]
    fn leverage_2x() {
        assert_eq!(leverage_to_margin_ratio(2.0).unwrap(), 500_000);
    }

    #[test]
    fn leverage_10x() {
        assert_eq!(leverage_to_margin_ratio(10.0).unwrap(), 100_000);
    }

    #[test]
    fn leverage_100x() {
        assert_eq!(leverage_to_margin_ratio(100.0).unwrap(), 10_000);
    }

    #[test]
    fn leverage_fractional_3x() {
        // 1e6 / 3 = 333333.33... → rounds to 333333
        assert_eq!(leverage_to_margin_ratio(3.0).unwrap(), 333_333);
    }

    #[test]
    fn leverage_zero_rejected() {
        assert!(leverage_to_margin_ratio(0.0).is_err());
    }

    #[test]
    fn leverage_negative_rejected() {
        assert!(leverage_to_margin_ratio(-5.0).is_err());
    }

    #[test]
    fn leverage_nan_rejected() {
        assert!(leverage_to_margin_ratio(f64::NAN).is_err());
    }

    #[test]
    fn leverage_infinity_rejected() {
        assert!(leverage_to_margin_ratio(f64::INFINITY).is_err());
    }

    #[test]
    fn leverage_extremely_large_produces_error() {
        // 1e6 / 1e12 rounds to 0 → error
        assert!(leverage_to_margin_ratio(1e12).is_err());
    }

    // ── margin_ratio_to_leverage ───────────────────────────────────

    #[test]
    fn margin_ratio_10_percent() {
        let lev = margin_ratio_to_leverage(100_000).unwrap();
        assert!((lev - 10.0).abs() < 1e-10);
    }

    #[test]
    fn margin_ratio_100_percent() {
        let lev = margin_ratio_to_leverage(1_000_000).unwrap();
        assert!((lev - 1.0).abs() < 1e-10);
    }

    #[test]
    fn margin_ratio_1_percent() {
        let lev = margin_ratio_to_leverage(10_000).unwrap();
        assert!((lev - 100.0).abs() < 1e-10);
    }

    #[test]
    fn margin_ratio_zero_rejected() {
        assert!(margin_ratio_to_leverage(0).is_err());
    }

    // ── leverage ↔ margin_ratio roundtrip ──────────────────────────

    #[test]
    fn leverage_margin_ratio_roundtrip() {
        for &lev in &[1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0] {
            let ratio = leverage_to_margin_ratio(lev).unwrap();
            let recovered = margin_ratio_to_leverage(ratio).unwrap();
            assert!(
                (recovered - lev).abs() < 0.01,
                "roundtrip failed for {lev}x: ratio={ratio}, recovered={recovered}"
            );
        }
    }

    // ── price_to_sqrt_price_x96 ────────────────────────────────────

    #[test]
    fn price_1_gives_approx_q96() {
        let result = price_to_sqrt_price_x96(1.0).unwrap();
        // sqrt(1) * 2^96 = Q96 exactly. With 6-decimal intermediate,
        // we expect precision within Q96 / 1e6.
        let diff = result.abs_diff(Q96);
        assert!(
            diff < Q96 / U256::from(1_000_000u64),
            "price=1.0 gave sqrtPriceX96={result}, expected ≈{Q96}"
        );
    }

    #[test]
    fn price_4_gives_approx_2_times_q96() {
        // sqrt(4) = 2, so sqrtPriceX96 ≈ 2 * Q96
        let result = price_to_sqrt_price_x96(4.0).unwrap();
        let expected = Q96 * U256::from(2u64);
        let diff = result.abs_diff(expected);
        assert!(
            diff < Q96 / U256::from(1_000_000u64),
            "price=4.0 gave sqrtPriceX96={result}, expected ≈{expected}"
        );
    }

    #[test]
    fn price_0_25_gives_approx_half_q96() {
        // sqrt(0.25) = 0.5, so sqrtPriceX96 ≈ Q96 / 2
        let result = price_to_sqrt_price_x96(0.25).unwrap();
        let expected = Q96 / U256::from(2u64);
        let diff = result.abs_diff(expected);
        assert!(
            diff < Q96 / U256::from(1_000_000u64),
            "price=0.25 gave sqrtPriceX96={result}, expected ≈{expected}"
        );
    }

    #[test]
    fn price_zero_rejected() {
        assert!(price_to_sqrt_price_x96(0.0).is_err());
    }

    #[test]
    fn price_negative_rejected() {
        assert!(price_to_sqrt_price_x96(-1.0).is_err());
    }

    #[test]
    fn price_too_large_rejected() {
        assert!(price_to_sqrt_price_x96(1e31).is_err());
    }

    #[test]
    fn price_nan_rejected() {
        assert!(price_to_sqrt_price_x96(f64::NAN).is_err());
    }

    #[test]
    fn price_infinity_rejected() {
        assert!(price_to_sqrt_price_x96(f64::INFINITY).is_err());
    }

    #[test]
    fn price_very_small_works() {
        // 0.001 is at the protocol minimum
        let result = price_to_sqrt_price_x96(0.001);
        assert!(result.is_ok());
    }

    #[test]
    fn price_1000_works() {
        // 1000 is at the protocol maximum
        let result = price_to_sqrt_price_x96(1000.0);
        assert!(result.is_ok());
    }

    // ── sqrt_price_x96_to_price ────────────────────────────────────

    #[test]
    fn sqrt_price_x96_q96_gives_price_1() {
        let price = sqrt_price_x96_to_price(Q96).unwrap();
        assert!(
            (price - 1.0).abs() < 0.000001,
            "sqrtPriceX96=Q96 gave price={price}, expected ≈1.0"
        );
    }

    #[test]
    fn sqrt_price_x96_2q96_gives_price_4() {
        let price = sqrt_price_x96_to_price(Q96 * U256::from(2u64)).unwrap();
        assert!(
            (price - 4.0).abs() < 0.001,
            "sqrtPriceX96=2*Q96 gave price={price}, expected ≈4.0"
        );
    }

    #[test]
    fn sqrt_price_x96_zero_rejected() {
        assert!(sqrt_price_x96_to_price(U256::ZERO).is_err());
    }

    #[test]
    fn sqrt_price_x96_protocol_min() {
        // MIN_SQRT_PRICE_X96 corresponds to price ≈ 0.001
        let price = sqrt_price_x96_to_price(MIN_SQRT_PRICE_X96).unwrap();
        assert!(
            (price - 0.001).abs() < 0.0005,
            "MIN_SQRT_PRICE_X96 gave price={price}, expected ≈0.001"
        );
    }

    #[test]
    fn sqrt_price_x96_protocol_max() {
        // MAX_SQRT_PRICE_X96 corresponds to price ≈ 1000
        let price = sqrt_price_x96_to_price(MAX_SQRT_PRICE_X96).unwrap();
        assert!(
            (price - 1000.0).abs() < 0.5,
            "MAX_SQRT_PRICE_X96 gave price={price}, expected ≈1000"
        );
    }

    // ── price ↔ sqrtPriceX96 roundtrip ─────────────────────────────

    #[test]
    fn price_sqrt_price_x96_roundtrip() {
        // Test a range of prices. The 6-decimal intermediate means we
        // lose precision at around 1e-6 relative error.
        for &price in &[0.01, 0.1, 0.5, 1.0, 2.0, 10.0, 100.0, 500.0, 999.0] {
            let sqrt_px96 = price_to_sqrt_price_x96(price).unwrap();
            let recovered = sqrt_price_x96_to_price(sqrt_px96).unwrap();
            let rel_error = (recovered - price).abs() / price;
            assert!(
                rel_error < 0.001,
                "roundtrip failed for price={price}: recovered={recovered}, \
                 relative error={rel_error}"
            );
        }
    }

    #[test]
    fn price_sqrt_price_x96_roundtrip_near_1() {
        // Prices near 1.0 should roundtrip with very high precision.
        let price = 1.05;
        let sqrt_px96 = price_to_sqrt_price_x96(price).unwrap();
        let recovered = sqrt_price_x96_to_price(sqrt_px96).unwrap();
        let rel_error = (recovered - price).abs() / price;
        assert!(
            rel_error < 0.0001,
            "price={price}: recovered={recovered}, rel_error={rel_error}"
        );
    }

    // ── Combined conversion scenarios ──────────────────────────────

    #[test]
    fn margin_scale_roundtrip_100_usdc() {
        let margin_usdc = 100.0;
        let on_chain = scale_to_6dec(margin_usdc).unwrap();
        assert_eq!(on_chain, 100_000_000);
        let back = scale_from_6dec(on_chain);
        assert_eq!(back, 100.0);
    }

    #[test]
    fn leverage_10x_to_ratio_and_back() {
        let ratio = leverage_to_margin_ratio(10.0).unwrap();
        assert_eq!(ratio, 100_000);
        let lev = margin_ratio_to_leverage(ratio).unwrap();
        assert!((lev - 10.0).abs() < 1e-10);
    }

    #[test]
    fn five_usdc_minimum_margin() {
        // Protocol minimum: 5 USDC = 5_000_000 on-chain
        let on_chain = scale_to_6dec(5.0).unwrap();
        assert_eq!(on_chain, 5_000_000);
    }

    // ── Overflow protection for sqrtPriceX96 squaring ──────────────

    #[test]
    fn sqrt_price_x96_huge_value_checked() {
        // A value close to U256::MAX / 2 should overflow when squared.
        let huge = U256::MAX / U256::from(2u64);
        let result = sqrt_price_x96_to_price(huge);
        assert!(result.is_err(), "should overflow when squaring huge value");
    }
}
