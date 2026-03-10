//! Position-level math: entry price, size, value, leverage, liquidation price.
//!
//! All functions are pure and take Alloy primitives (`I256`) for on-chain
//! signed values and `f64` for pre-converted human-readable values (margin,
//! ratios). No structs — just functions.
//!
//! # On-chain representation
//!
//! Position deltas are signed 256-bit integers scaled by 1e6:
//! - `entry_perp_delta`: positive = long, negative = short (in base units × 1e6)
//! - `entry_usd_delta`: positive = received USD, negative = paid USD (in USDC × 1e6)
//!
//! Margin and fee ratios are pre-converted to `f64` by the caller.

use alloy::primitives::I256;

/// Scale factor for on-chain 6-decimal values.
const SCALE_1E6: f64 = 1_000_000.0;

/// Convert an `I256` to `f64`.
///
/// For values that fit in `i128` (which covers all practical position sizes),
/// this is a direct cast. Returns `±inf` for values beyond `i128` range.
// Was not inlined — 2 function calls per entry_price. Now inlined: ~2ns saved per call.
#[inline]
fn i256_to_f64(x: I256) -> f64 {
    // Fast path: all realistic position sizes fit in i64 → single scvtf instruction
    if let Ok(narrow) = i64::try_from(x) {
        return narrow as f64;
    }
    i256_to_f64_slow(x)
}

/// Slow path for I256 → f64 conversion (values beyond i64 range).
#[cold]
#[inline(never)]
fn i256_to_f64_slow(x: I256) -> f64 {
    if let Ok(narrow) = i128::try_from(x) {
        return narrow as f64;
    }
    // Fallback: convert via absolute value.
    let is_neg = x.is_negative();
    let abs = x.unsigned_abs();
    if let Ok(narrow) = u128::try_from(abs) {
        let f = narrow as f64;
        return if is_neg { -f } else { f };
    }
    // Beyond u128 range: return infinity as a sentinel.
    if is_neg {
        f64::NEG_INFINITY
    } else {
        f64::INFINITY
    }
}

/// Calculate the entry price of a position.
///
/// ```text
/// entry_price = |entry_usd_delta| / |entry_perp_delta|
/// ```
///
/// Both deltas are scaled by 1e6, so the ratio gives the price directly
/// (the scaling factors cancel out).
///
/// Returns `0.0` if `entry_perp_delta` is zero.
///
/// # Examples
///
/// ```
/// # use perpcity_rust_sdk::math::position::entry_price;
/// # use alloy::primitives::I256;
/// // 1 ETH at $1500: perp_delta = 1e6, usd_delta = -1500e6
/// let price = entry_price(
///     I256::try_from(1_000_000i64).unwrap(),
///     I256::try_from(-1_500_000_000i64).unwrap(),
/// );
/// assert!((price - 1500.0).abs() < 0.001);
/// ```
#[inline]
pub fn entry_price(entry_perp_delta: I256, entry_usd_delta: I256) -> f64 {
    let perp_f = i256_to_f64(entry_perp_delta);
    if perp_f == 0.0 {
        return 0.0;
    }
    i256_to_f64(entry_usd_delta).abs() / perp_f.abs()
}

/// Calculate the position size in base units (not scaled).
///
/// ```text
/// size = entry_perp_delta / 1e6
/// ```
///
/// Positive = long, negative = short.
///
/// # Examples
///
/// ```
/// # use perpcity_rust_sdk::math::position::position_size;
/// # use alloy::primitives::I256;
/// let size = position_size(I256::try_from(2_500_000i64).unwrap());
/// assert!((size - 2.5).abs() < 1e-6);
/// ```
#[inline]
pub fn position_size(entry_perp_delta: I256) -> f64 {
    i256_to_f64(entry_perp_delta) / SCALE_1E6
}

/// Calculate the current position value at a given mark price.
///
/// ```text
/// value = |size| × mark_price
/// ```
///
/// # Examples
///
/// ```
/// # use perpcity_rust_sdk::math::position::position_value;
/// # use alloy::primitives::I256;
/// let val = position_value(I256::try_from(1_000_000i64).unwrap(), 1600.0);
/// assert!((val - 1600.0).abs() < 0.001);
/// ```
#[inline]
pub fn position_value(entry_perp_delta: I256, mark_price: f64) -> f64 {
    let size = position_size(entry_perp_delta);
    size.abs() * mark_price
}

/// Calculate the leverage of a position.
///
/// ```text
/// leverage = position_value / effective_margin
/// ```
///
/// Returns `f64::INFINITY` if effective margin is ≤ 0.
///
/// # Examples
///
/// ```
/// # use perpcity_rust_sdk::math::position::leverage;
/// let lev = leverage(1000.0, 100.0);
/// assert!((lev - 10.0).abs() < 0.001);
/// ```
#[inline]
pub fn leverage(position_value: f64, effective_margin: f64) -> f64 {
    if effective_margin <= 0.0 {
        return f64::INFINITY;
    }
    position_value / effective_margin
}

/// Calculate the liquidation price of a position.
///
/// Returns `None` if size is zero or margin ≤ 0.
///
/// For **long** positions (`is_long = true`):
/// ```text
/// liq_price = entry_price − (margin − liq_ratio × notional) / |size|
/// ```
/// Clamped to ≥ 0 (price can't go negative).
///
/// For **short** positions (`is_long = false`):
/// ```text
/// liq_price = entry_price + (margin − liq_ratio × notional) / |size|
/// ```
///
/// # Arguments
///
/// - `entry_perp_delta`, `entry_usd_delta`: On-chain signed deltas (I256, scaled 1e6)
/// - `margin`: Current margin in USDC (human-readable f64)
/// - `liq_ratio_scaled`: Liquidation margin ratio, scaled by 1e6 (e.g. `25_000` = 2.5%)
/// - `is_long`: Whether this is a long position
///
/// # Examples
///
/// ```
/// # use perpcity_rust_sdk::math::position::liquidation_price;
/// # use alloy::primitives::I256;
/// // Long 1 ETH at $1500, $100 margin, 2.5% liq ratio
/// let liq = liquidation_price(
///     I256::try_from(1_000_000i64).unwrap(),
///     I256::try_from(-1_500_000_000i64).unwrap(),
///     100.0,
///     25_000,
///     true,
/// );
/// assert!((liq.unwrap() - 1437.5).abs() < 0.01);
/// ```
#[inline]
pub fn liquidation_price(
    entry_perp_delta: I256,
    entry_usd_delta: I256,
    margin: f64,
    liq_ratio_scaled: u32,
    is_long: bool,
) -> Option<f64> {
    let size = position_size(entry_perp_delta);
    if size == 0.0 {
        return None;
    }
    if margin <= 0.0 {
        return None;
    }

    let ep = entry_price(entry_perp_delta, entry_usd_delta);
    let abs_size = size.abs();
    let notional = abs_size * ep;
    let liq_ratio = liq_ratio_scaled as f64 / SCALE_1E6;

    let margin_excess = margin - liq_ratio * notional;

    if is_long {
        let liq = ep - margin_excess / abs_size;
        Some(liq.max(0.0))
    } else {
        let liq = ep + margin_excess / abs_size;
        Some(liq)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to make I256 from i64 without verbosity.
    fn i(val: i64) -> I256 {
        I256::try_from(val).unwrap()
    }

    // ── i256_to_f64 ──────────────────────────────────────────────

    #[test]
    fn i256_to_f64_positive() {
        assert!((i256_to_f64(i(1_000_000)) - 1_000_000.0).abs() < 0.5);
    }

    #[test]
    fn i256_to_f64_negative() {
        assert!((i256_to_f64(i(-1_000_000)) - (-1_000_000.0)).abs() < 0.5);
    }

    #[test]
    fn i256_to_f64_zero() {
        assert_eq!(i256_to_f64(I256::ZERO), 0.0);
    }

    #[test]
    fn i256_to_f64_beyond_i128() {
        // Positive beyond i128 but within u128: the unsigned fallback path
        // must produce a finite result (not the infinity sentinel).
        let beyond_i128 = I256::try_from(i128::MAX).unwrap() + I256::try_from(1i64).unwrap();
        let f = i256_to_f64(beyond_i128);
        assert!(f.is_finite());
        assert!(f > 0.0);

        // Beyond u128 range entirely: returns infinity sentinel.
        assert_eq!(i256_to_f64(I256::MAX), f64::INFINITY);
        assert_eq!(i256_to_f64(I256::MIN), f64::NEG_INFINITY);
    }

    // ── entry_price ──────────────────────────────────────────────

    #[test]
    fn entry_price_basic() {
        // 1 ETH at 1500 USDC: perp_delta = 1e6, usd_delta = -1500e6
        let price = entry_price(i(1_000_000), i(-1_500_000_000));
        assert!(
            (price - 1500.0).abs() < 0.001,
            "price={price}, expected 1500"
        );
    }

    #[test]
    fn entry_price_short() {
        // Short 1 ETH at 1500: perp_delta = -1e6, usd_delta = +1500e6
        let price = entry_price(i(-1_000_000), i(1_500_000_000));
        assert!(
            (price - 1500.0).abs() < 0.001,
            "price={price}, expected 1500"
        );
    }

    #[test]
    fn entry_price_fractional() {
        // 0.5 ETH at 2000: perp_delta = 500_000, usd_delta = -1_000_000_000
        let price = entry_price(i(500_000), i(-1_000_000_000));
        assert!(
            (price - 2000.0).abs() < 0.001,
            "price={price}, expected 2000"
        );
    }

    #[test]
    fn entry_price_zero_perp_returns_zero() {
        assert_eq!(entry_price(I256::ZERO, I256::ZERO), 0.0);
    }

    // ── position_size ────────────────────────────────────────────

    #[test]
    fn position_size_basic() {
        let size = position_size(i(2_500_000));
        assert!((size - 2.5).abs() < 1e-6);
    }

    #[test]
    fn position_size_negative() {
        let size = position_size(i(-1_000_000));
        assert!((size - (-1.0)).abs() < 1e-6);
    }

    #[test]
    fn position_size_zero() {
        assert_eq!(position_size(I256::ZERO), 0.0);
    }

    #[test]
    fn position_size_fractional_eth() {
        // 0.001 ETH = 1000 on-chain
        let size = position_size(i(1_000));
        assert!((size - 0.001).abs() < 1e-9);
    }

    // ── position_value ───────────────────────────────────────────

    #[test]
    fn position_value_basic() {
        // 1 ETH at mark price 1600 → value = 1600
        let val = position_value(i(1_000_000), 1600.0);
        assert!((val - 1600.0).abs() < 0.001);
    }

    #[test]
    fn position_value_short() {
        // Short 1 ETH at mark 1600 → value still 1600 (absolute)
        let val = position_value(i(-1_000_000), 1600.0);
        assert!((val - 1600.0).abs() < 0.001);
    }

    #[test]
    fn position_value_half_eth() {
        let val = position_value(i(500_000), 2000.0);
        assert!((val - 1000.0).abs() < 0.001);
    }

    // ── leverage ─────────────────────────────────────────────────

    #[test]
    fn leverage_basic() {
        let lev = leverage(1000.0, 100.0);
        assert!((lev - 10.0).abs() < 0.001);
    }

    #[test]
    fn leverage_1x() {
        let lev = leverage(1000.0, 1000.0);
        assert!((lev - 1.0).abs() < 0.001);
    }

    #[test]
    fn leverage_zero_margin() {
        assert!(leverage(1000.0, 0.0).is_infinite());
    }

    #[test]
    fn leverage_negative_margin() {
        assert!(leverage(1000.0, -50.0).is_infinite());
    }

    // ── liquidation_price ────────────────────────────────────────

    #[test]
    fn liquidation_price_long() {
        // Long 1 ETH at $1500, $100 margin, 2.5% liq ratio
        // liq = 1500 - (100 - 0.025 * 1500) / 1 = 1500 - 62.5 = 1437.5
        let liq = liquidation_price(i(1_000_000), i(-1_500_000_000), 100.0, 25_000, true);
        assert!(liq.is_some());
        assert!(
            (liq.unwrap() - 1437.5).abs() < 0.01,
            "liq={}, expected 1437.5",
            liq.unwrap()
        );
    }

    #[test]
    fn liquidation_price_short() {
        // Short 1 ETH at $1500, $100 margin, 2.5% liq ratio
        // liq = 1500 + (100 - 0.025 * 1500) / 1 = 1500 + 62.5 = 1562.5
        let liq = liquidation_price(i(-1_000_000), i(1_500_000_000), 100.0, 25_000, false);
        assert!(liq.is_some());
        assert!(
            (liq.unwrap() - 1562.5).abs() < 0.01,
            "liq={}, expected 1562.5",
            liq.unwrap()
        );
    }

    #[test]
    fn liquidation_price_long_clamped_to_zero() {
        // If margin is so large that liq_price would go negative, clamp to 0.
        // 1 ETH at $100, $200 margin, 2.5% liq ratio
        // liq = 100 - (200 - 0.025 * 100) / 1 = 100 - 197.5 = -97.5 → clamped to 0
        let liq = liquidation_price(i(1_000_000), i(-100_000_000), 200.0, 25_000, true);
        assert!(liq.is_some());
        assert_eq!(liq.unwrap(), 0.0);
    }

    #[test]
    fn liquidation_price_zero_size() {
        assert_eq!(
            liquidation_price(I256::ZERO, I256::ZERO, 100.0, 25_000, true),
            None
        );
    }

    #[test]
    fn liquidation_price_zero_margin() {
        assert_eq!(
            liquidation_price(i(1_000_000), i(-1_500_000_000), 0.0, 25_000, true),
            None
        );
    }

    #[test]
    fn liquidation_price_negative_margin() {
        assert_eq!(
            liquidation_price(i(1_000_000), i(-1_500_000_000), -50.0, 25_000, true),
            None
        );
    }

    #[test]
    fn liquidation_price_high_leverage_long() {
        // 1 ETH at $1500, $15 margin (100x leverage), 2.5% liq ratio
        // liq = 1500 - (15 - 0.025 * 1500) / 1 = 1500 - (15 - 37.5) = 1500 + 22.5 = 1522.5
        // With extremely high leverage, liq price is ABOVE entry (very close to liquidation).
        let liq = liquidation_price(i(1_000_000), i(-1_500_000_000), 15.0, 25_000, true);
        assert!(liq.is_some());
        let liq_val = liq.unwrap();
        assert!(
            (liq_val - 1522.5).abs() < 0.01,
            "liq={liq_val}, expected 1522.5"
        );
        // Liq price is above entry price — position is almost liquidated immediately.
        assert!(liq_val > 1500.0);
    }

    #[test]
    fn liquidation_price_5_percent_ratio() {
        // Long 2 ETH at $1000, $200 margin, 5% liq ratio
        // notional = 2 * 1000 = 2000
        // liq = 1000 - (200 - 0.05 * 2000) / 2 = 1000 - (200 - 100) / 2 = 1000 - 50 = 950
        let liq = liquidation_price(i(2_000_000), i(-2_000_000_000), 200.0, 50_000, true);
        assert!(liq.is_some());
        assert!(
            (liq.unwrap() - 950.0).abs() < 0.01,
            "liq={}, expected 950",
            liq.unwrap()
        );
    }
}
