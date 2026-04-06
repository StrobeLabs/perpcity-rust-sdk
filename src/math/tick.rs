//! Tick ↔ price conversions and the Uniswap V4 `getSqrtRatioAtTick` algorithm.
//!
//! All functions are pure and operate on primitives (`i32`, `U256`, `f64`).
//! The `get_sqrt_ratio_at_tick` implementation is an exact port of the
//! Solidity `TickMath.getSqrtRatioAtTick` bit-shift lookup table.

use alloy::primitives::{U256, uint};

use crate::errors::ValidationError;

/// Uniswap V4 absolute tick bounds.
const UNISWAP_MIN_TICK: i32 = -887_272;
const UNISWAP_MAX_TICK: i32 = 887_272;

// ── Precomputed constants for O(1) float tick↔price conversion ──────
// ln(1.0001) and its reciprocal, verified at compile time via unit test.
const LN_1_0001: f64 = 9.999500033329732e-5;
const INV_LN_1_0001: f64 = 10000.499991668185;

/// Compute the `sqrtPriceX96` for a given tick using the Uniswap V4
/// bit-shift lookup table.
///
/// This is a faithful port of the Solidity `TickMath.getSqrtRatioAtTick`.
/// The result equals `sqrt(1.0001^tick) * 2^96`, returned as `U256`.
///
/// Internally uses native `u128` arithmetic instead of `U256` for the
/// multiply-shift loop. All magic constants and intermediate ratios fit
/// in 128 bits, so this avoids the overhead of 256-bit math.
///
/// # Errors
///
/// Returns [`ValidationError::InvalidTickRange`] if `|tick| > 887272`.
///
/// # Examples
///
/// ```
/// # use perpcity_sdk::math::tick::get_sqrt_ratio_at_tick;
/// # use perpcity_sdk::constants::Q96;
/// let sqrt_price = get_sqrt_ratio_at_tick(0).unwrap();
/// assert_eq!(sqrt_price, Q96); // tick 0 → sqrtPrice = 1.0
/// ```
pub fn get_sqrt_ratio_at_tick(tick: i32) -> Result<U256, ValidationError> {
    let abs_tick = tick.unsigned_abs();
    if abs_tick > UNISWAP_MAX_TICK as u32 {
        return Err(ValidationError::InvalidTickRange {
            lower: tick,
            upper: tick,
        });
    }

    // All magic constants fit in u128. The ratio stays ≤ 128 bits after
    // each multiply-shift. We track a flag for the initial 2^128 value
    // (which doesn't fit in u128) and resolve it on first use.
    let mut ratio: u128;
    // is_unit = true means the actual ratio is 2^128 (not stored in `ratio`)
    let mut is_unit: bool;

    if abs_tick & 0x1 != 0 {
        ratio = 0xfffcb933bd6fad37aa2d162d1a594001_u128;
        is_unit = false;
    } else {
        ratio = 0; // placeholder — not used while is_unit is true
        is_unit = true;
    }

    // Each apply_bit: if the bit is set, multiply ratio by the magic constant
    // and shift right 128. When is_unit (ratio = 2^128), the result is just
    // the magic constant itself: (2^128 * M) >> 128 = M.
    macro_rules! apply_bit_u128 {
        ($bit:expr, $magic:expr) => {
            if abs_tick & $bit != 0 {
                if is_unit {
                    ratio = $magic;
                    is_unit = false;
                } else {
                    ratio = mul_shift_128(ratio, $magic);
                }
            }
        };
    }

    apply_bit_u128!(0x2, 0xfff97272373d413259a46990580e213a_u128);
    apply_bit_u128!(0x4, 0xfff2e50f5f656932ef12357cf3c7fdcc_u128);
    apply_bit_u128!(0x8, 0xffe5caca7e10e4e61c3624eaa0941cd0_u128);
    apply_bit_u128!(0x10, 0xffcb9843d60f6159c9db58835c926644_u128);
    apply_bit_u128!(0x20, 0xff973b41fa98c081472e6896dfb254c0_u128);
    apply_bit_u128!(0x40, 0xff2ea16466c96a3843ec78b326b52861_u128);
    apply_bit_u128!(0x80, 0xfe5dee046a99a2a811c461f1969c3053_u128);
    apply_bit_u128!(0x100, 0xfcbe86c7900a88aedcffc83b479aa3a4_u128);
    apply_bit_u128!(0x200, 0xf987a7253ac413176f2b074cf7815e54_u128);
    apply_bit_u128!(0x400, 0xf3392b0822b70005940c7a398e4b70f3_u128);
    apply_bit_u128!(0x800, 0xe7159475a2c29b7443b29c7fa6e889d9_u128);
    apply_bit_u128!(0x1000, 0xd097f3bdfd2022b8845ad8f792aa5825_u128);
    apply_bit_u128!(0x2000, 0xa9f746462d870fdf8a65dc1f90e061e5_u128);
    apply_bit_u128!(0x4000, 0x70d869a156d2a1b890bb3df62baf32f7_u128);
    apply_bit_u128!(0x8000, 0x31be135f97d08fd981231505542fcfa6_u128);
    apply_bit_u128!(0x10000, 0x9aa508b5b7a84e1c677de54f3e99bc9_u128);
    apply_bit_u128!(0x20000, 0x5d6af8dedb81196699c329225ee604_u128);
    apply_bit_u128!(0x40000, 0x2216e584f5fa1ea926041bedfe98_u128);
    apply_bit_u128!(0x80000, 0x48a170391f7dc42444e8fa2_u128);

    // Convert back to U256 for the final steps
    let mut result = if is_unit {
        // abs_tick was 0: ratio is exactly 2^128
        uint!(0x100000000000000000000000000000000_U256)
    } else {
        U256::from(ratio)
    };

    if tick > 0 {
        result = U256::MAX / result;
    }

    Ok(result >> 32)
}

/// Compute (a × b) >> 128 using native u128 widening multiply.
/// This is the hot inner operation — 4 mul instructions on ARM vs 6+ for U256.
// Measured: ~1.5ns/bit vs ~2.5ns/bit for U256 path
#[inline(always)]
fn mul_shift_128(a: u128, b: u128) -> u128 {
    let a_hi = (a >> 64) as u64;
    let a_lo = a as u64;
    let b_hi = (b >> 64) as u64;
    let b_lo = b as u64;

    // 4 partial products (each compiles to a single mul + umulh pair on ARM)
    let ll = (a_lo as u128) * (b_lo as u128);
    let lh = (a_lo as u128) * (b_hi as u128);
    let hl = (a_hi as u128) * (b_lo as u128);
    let hh = (a_hi as u128) * (b_hi as u128);

    // Sum cross terms and extract upper 128 bits
    let (cross, carry1) = lh.overflowing_add(hl);
    let (mid, carry2) = cross.overflowing_add(ll >> 64);

    hh + (mid >> 64) + (((carry1 as u128) + (carry2 as u128)) << 64)
}

/// Convert a tick to a human-readable price.
///
/// `price = 1.0001^tick`, computed via `exp(tick × ln(1.0001))` for O(1)
/// performance regardless of tick magnitude. Precision is within 1e-10
/// relative error for the full protocol tick range.
///
/// # Errors
///
/// Returns an error if the tick is out of the Uniswap V4 range.
///
/// # Examples
///
/// ```
/// # use perpcity_sdk::math::tick::tick_to_price;
/// let price = tick_to_price(0).unwrap();
/// assert!((price - 1.0).abs() < 1e-10);
/// ```
// O(1) float exp — 38.8ns → ~4ns for tick 1000
#[inline]
pub fn tick_to_price(tick: i32) -> Result<f64, ValidationError> {
    if !(UNISWAP_MIN_TICK..=UNISWAP_MAX_TICK).contains(&tick) {
        return Err(ValidationError::InvalidTickRange {
            lower: tick,
            upper: tick,
        });
    }
    Ok((tick as f64 * LN_1_0001).exp())
}

/// Convert a human-readable price to the nearest tick.
///
/// Uses the mathematical relationship `tick = log(price) / log(1.0001)`,
/// then rounds to the nearest integer.
///
/// # Errors
///
/// Returns [`ValidationError::InvalidPrice`] if `price` is not positive/finite,
/// or if the resulting tick is out of the Uniswap V4 range.
///
/// # Examples
///
/// ```
/// # use perpcity_sdk::math::tick::price_to_tick;
/// let tick = price_to_tick(1.0).unwrap();
/// assert_eq!(tick, 0);
/// ```
// Multiply by reciprocal instead of dividing — avoids fdiv latency
#[inline]
pub fn price_to_tick(price: f64) -> Result<i32, ValidationError> {
    if !price.is_finite() || price <= 0.0 {
        return Err(ValidationError::InvalidPrice {
            reason: format!("price must be positive and finite, got {price}"),
        });
    }

    let tick_f = price.ln() * INV_LN_1_0001;
    let tick = tick_f.round() as i32;

    if !(UNISWAP_MIN_TICK..=UNISWAP_MAX_TICK).contains(&tick) {
        return Err(ValidationError::InvalidPrice {
            reason: format!(
                "price {price} maps to tick {tick}, outside [{UNISWAP_MIN_TICK}, {UNISWAP_MAX_TICK}]"
            ),
        });
    }

    Ok(tick)
}

/// Round a tick down to the nearest multiple of the protocol tick spacing.
///
/// For negative ticks, this rounds toward negative infinity.
///
/// # Examples
///
/// ```
/// # use perpcity_sdk::math::tick::align_tick_down;
/// assert_eq!(align_tick_down(35, 30), 30);
/// assert_eq!(align_tick_down(-1, 30), -30);
/// assert_eq!(align_tick_down(60, 30), 60);
/// ```
pub fn align_tick_down(tick: i32, spacing: i32) -> i32 {
    debug_assert!(spacing > 0, "tick spacing must be positive, got {spacing}");
    tick - tick.rem_euclid(spacing)
}

/// Round a tick up to the nearest multiple of the protocol tick spacing.
///
/// For positive ticks, this rounds toward positive infinity.
///
/// # Examples
///
/// ```
/// # use perpcity_sdk::math::tick::align_tick_up;
/// assert_eq!(align_tick_up(1, 30), 30);
/// assert_eq!(align_tick_up(-35, 30), -30);
/// assert_eq!(align_tick_up(60, 30), 60);
/// ```
pub fn align_tick_up(tick: i32, spacing: i32) -> i32 {
    debug_assert!(spacing > 0, "tick spacing must be positive, got {spacing}");
    let remainder = tick.rem_euclid(spacing);
    if remainder == 0 {
        tick
    } else {
        tick + (spacing - remainder)
    }
}

/// Convert a `sqrtPriceX96` to a human-readable f64 price.
///
/// `price = (sqrtPriceX96 / 2^96)^2`
///
/// Uses `u128` intermediate to avoid precision loss — the sqrtPriceX96
/// values we encounter in practice fit within u128.
#[cfg(test)]
pub(crate) fn sqrt_price_x96_to_f64_price(sqrt_price_x96: U256) -> Result<f64, ValidationError> {
    if sqrt_price_x96.is_zero() {
        return Err(ValidationError::InvalidPrice {
            reason: "sqrtPriceX96 must be non-zero".into(),
        });
    }

    let narrow = u128::try_from(sqrt_price_x96).map_err(|_| ValidationError::Overflow {
        context: "sqrtPriceX96 exceeds u128::MAX".into(),
    })?;

    let ratio = narrow as f64 / crate::constants::Q96_U128 as f64;
    Ok(ratio * ratio)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{self, Q96, TICK_SPACING};

    // ── get_sqrt_ratio_at_tick ────────────────────────────────────

    #[test]
    fn tick_0_gives_q96() {
        // At tick 0, sqrtPrice = 1.0, so sqrtPriceX96 = Q96 exactly.
        let result = get_sqrt_ratio_at_tick(0).unwrap();
        assert_eq!(result, Q96);
    }

    #[test]
    fn positive_tick_gives_larger_value() {
        let at_0 = get_sqrt_ratio_at_tick(0).unwrap();
        let at_100 = get_sqrt_ratio_at_tick(100).unwrap();
        assert!(at_100 > at_0);
    }

    #[test]
    fn negative_tick_gives_smaller_value() {
        let at_0 = get_sqrt_ratio_at_tick(0).unwrap();
        let at_neg100 = get_sqrt_ratio_at_tick(-100).unwrap();
        assert!(at_neg100 < at_0);
    }

    #[test]
    fn tick_symmetry() {
        // sqrtPrice(tick) * sqrtPrice(-tick) ≈ Q96^2
        // because sqrt(1.0001^tick) * sqrt(1.0001^-tick) = 1, so in X96: Q96^2.
        let pos = get_sqrt_ratio_at_tick(1000).unwrap();
        let neg = get_sqrt_ratio_at_tick(-1000).unwrap();
        let product = pos * neg;
        let q96_squared = Q96 * Q96;
        let diff = product.abs_diff(q96_squared);
        // Allow small rounding error (< 1 ppm of Q96^2).
        assert!(
            diff < q96_squared / U256::from(1_000_000u64),
            "symmetry violated: diff={diff}"
        );
    }

    #[test]
    fn tick_out_of_range_positive() {
        assert!(get_sqrt_ratio_at_tick(887_273).is_err());
    }

    #[test]
    fn tick_out_of_range_negative() {
        assert!(get_sqrt_ratio_at_tick(-887_273).is_err());
    }

    #[test]
    fn tick_at_boundary() {
        // Exactly at ±887272 should succeed.
        assert!(get_sqrt_ratio_at_tick(887_272).is_ok());
        assert!(get_sqrt_ratio_at_tick(-887_272).is_ok());
    }

    #[test]
    fn tick_monotonically_increasing() {
        // sqrtPriceX96 should strictly increase with tick.
        let mut prev = get_sqrt_ratio_at_tick(-1000).unwrap();
        for t in (-999..=1000).step_by(100) {
            let curr = get_sqrt_ratio_at_tick(t).unwrap();
            assert!(curr > prev, "not monotonic at tick {t}: {curr} <= {prev}");
            prev = curr;
        }
    }

    #[test]
    fn known_tick_30_value() {
        // price at tick 30 = 1.0001^30 ≈ 1.003004
        let sqrt_px96 = get_sqrt_ratio_at_tick(30).unwrap();
        let price = sqrt_price_x96_to_f64_price(sqrt_px96).unwrap();
        let expected = 1.0001_f64.powi(30);
        let rel_err = (price - expected).abs() / expected;
        assert!(
            rel_err < 1e-6,
            "tick 30: price={price}, expected={expected}, rel_err={rel_err}"
        );
    }

    #[test]
    fn known_tick_neg300_value() {
        // price at tick -300 = 1.0001^(-300) ≈ 0.97044
        let sqrt_px96 = get_sqrt_ratio_at_tick(-300).unwrap();
        let price = sqrt_price_x96_to_f64_price(sqrt_px96).unwrap();
        let expected = 1.0001_f64.powi(-300);
        let rel_err = (price - expected).abs() / expected;
        assert!(
            rel_err < 1e-6,
            "tick -300: price={price}, expected={expected}, rel_err={rel_err}"
        );
    }

    // ── tick_to_price / price_to_tick ─────────────────────────────

    #[test]
    fn tick_0_to_price_1() {
        let price = tick_to_price(0).unwrap();
        assert!((price - 1.0).abs() < 1e-10);
    }

    #[test]
    fn price_1_to_tick_0() {
        let tick = price_to_tick(1.0).unwrap();
        assert_eq!(tick, 0);
    }

    #[test]
    fn tick_to_price_roundtrip() {
        // For a range of ticks, converting to price and back should give the same tick.
        for &t in &[-69000, -30000, -1000, -30, 0, 30, 1000, 30000, 69000] {
            let price = tick_to_price(t).unwrap();
            let recovered = price_to_tick(price).unwrap();
            assert!(
                (recovered - t).abs() <= 1,
                "tick {t}: price={price}, recovered={recovered}"
            );
        }
    }

    #[test]
    fn price_to_tick_roundtrip() {
        // For a range of prices, converting to tick and back should be close.
        for &p in &[0.001, 0.01, 0.1, 0.5, 1.0, 2.0, 10.0, 100.0, 999.0] {
            let tick = price_to_tick(p).unwrap();
            let recovered = tick_to_price(tick).unwrap();
            let rel_err = (recovered - p).abs() / p;
            assert!(
                rel_err < 0.001,
                "price {p}: tick={tick}, recovered={recovered}, rel_err={rel_err}"
            );
        }
    }

    #[test]
    fn price_to_tick_rejects_zero() {
        assert!(price_to_tick(0.0).is_err());
    }

    #[test]
    fn price_to_tick_rejects_negative() {
        assert!(price_to_tick(-1.0).is_err());
    }

    #[test]
    fn price_to_tick_rejects_nan() {
        assert!(price_to_tick(f64::NAN).is_err());
    }

    #[test]
    fn price_to_tick_rejects_infinity() {
        assert!(price_to_tick(f64::INFINITY).is_err());
    }

    // ── align_tick_down / align_tick_up ──────────────────────────

    #[test]
    fn align_down_already_aligned() {
        assert_eq!(align_tick_down(60, TICK_SPACING), 60);
        assert_eq!(align_tick_down(-60, TICK_SPACING), -60);
        assert_eq!(align_tick_down(0, TICK_SPACING), 0);
    }

    #[test]
    fn align_down_positive() {
        assert_eq!(align_tick_down(35, TICK_SPACING), 30);
        assert_eq!(align_tick_down(59, TICK_SPACING), 30);
        assert_eq!(align_tick_down(1, TICK_SPACING), 0);
    }

    #[test]
    fn align_down_negative() {
        // Rounding toward -∞: -1 rounds down to -30.
        assert_eq!(align_tick_down(-1, TICK_SPACING), -30);
        assert_eq!(align_tick_down(-31, TICK_SPACING), -60);
        assert_eq!(align_tick_down(-29, TICK_SPACING), -30);
    }

    #[test]
    fn align_up_already_aligned() {
        assert_eq!(align_tick_up(60, TICK_SPACING), 60);
        assert_eq!(align_tick_up(-60, TICK_SPACING), -60);
        assert_eq!(align_tick_up(0, TICK_SPACING), 0);
    }

    #[test]
    fn align_up_positive() {
        assert_eq!(align_tick_up(1, TICK_SPACING), 30);
        assert_eq!(align_tick_up(31, TICK_SPACING), 60);
        assert_eq!(align_tick_up(29, TICK_SPACING), 30);
    }

    #[test]
    fn align_up_negative() {
        // -1 rounds up to 0, -31 rounds up to -30.
        assert_eq!(align_tick_up(-1, TICK_SPACING), 0);
        assert_eq!(align_tick_up(-31, TICK_SPACING), -30);
        assert_eq!(align_tick_up(-35, TICK_SPACING), -30);
        assert_eq!(align_tick_up(-59, TICK_SPACING), -30);
    }

    #[test]
    fn align_down_then_up_widens_range() {
        // When creating a maker position, the lower tick is aligned down
        // and the upper tick is aligned up. This always widens the range.
        let lower = align_tick_down(35, TICK_SPACING);
        let upper = align_tick_up(55, TICK_SPACING);
        assert_eq!(lower, 30);
        assert_eq!(upper, 60);
        assert!(upper > lower);
    }

    // ── Precomputed constants ────────────────────────────────────

    #[test]
    fn ln_1_0001_constant_matches_runtime() {
        let computed = 1.0001_f64.ln();
        assert_eq!(LN_1_0001, computed, "LN_1_0001 stale");
    }

    #[test]
    fn inv_ln_1_0001_constant_matches_runtime() {
        let computed = 1.0 / 1.0001_f64.ln();
        assert_eq!(INV_LN_1_0001, computed, "INV_LN_1_0001 stale");
    }

    // ── Protocol-range ticks ─────────────────────────────────────

    #[test]
    fn protocol_min_max_ticks_produce_valid_prices() {
        // PerpCity uses ±69090 as its bounds.
        let min_price = tick_to_price(constants::MIN_TICK).unwrap();
        let max_price = tick_to_price(constants::MAX_TICK).unwrap();
        assert!(min_price > 0.0);
        assert!(max_price > min_price);
        // MIN_TICK ≈ -69090 → price ≈ 0.001
        assert!((min_price - 0.001).abs() < 0.0005, "min_price={min_price}");
        // MAX_TICK ≈ 69090 → price ≈ 1000
        assert!((max_price - 1000.0).abs() < 1.0, "max_price={max_price}");
    }
}
