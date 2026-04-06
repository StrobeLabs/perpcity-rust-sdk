//! Liquidity estimation for PerpCity maker positions.
//!
//! These functions help determine how much liquidity to provide across a
//! tick range, either from a flat USD amount or targeting a specific margin
//! ratio.

use alloy::primitives::U256;

use crate::constants::Q96;
use crate::errors::ValidationError;

use super::tick::get_sqrt_ratio_at_tick;

/// Estimate the liquidity needed to deploy `usd_amount` of value across a
/// tick range `[tick_lower, tick_upper]`.
///
/// Uses the Uniswap V3/V4 formula for concentrated liquidity:
///
/// ```text
/// L = (usd_amount_scaled × 2^96) / (sqrtPriceUpper − sqrtPriceLower)
/// ```
///
/// where `usd_amount_scaled` is in 6-decimal units (1 USDC = 1_000_000).
///
/// # Errors
///
/// - [`ValidationError::InvalidTickRange`] if `tick_lower >= tick_upper`
/// - [`ValidationError::InvalidMargin`] if `usd_amount_scaled` is 0
/// - [`ValidationError::Overflow`] if the sqrt price delta is zero
pub fn estimate_liquidity(
    tick_lower: i32,
    tick_upper: i32,
    usd_amount_scaled: u128,
) -> Result<U256, ValidationError> {
    if tick_lower >= tick_upper {
        return Err(ValidationError::InvalidTickRange {
            lower: tick_lower,
            upper: tick_upper,
        });
    }
    if usd_amount_scaled == 0 {
        return Err(ValidationError::InvalidMargin {
            reason: "USD amount must be non-zero".into(),
        });
    }

    let sqrt_lower = get_sqrt_ratio_at_tick(tick_lower)?;
    let sqrt_upper = get_sqrt_ratio_at_tick(tick_upper)?;

    let delta = sqrt_upper - sqrt_lower;
    if delta.is_zero() {
        return Err(ValidationError::Overflow {
            context: "sqrtPrice delta is zero".into(),
        });
    }

    let numerator = U256::from(usd_amount_scaled) * Q96;
    Ok(numerator / delta)
}

/// Calculate the liquidity needed for a maker position given a target margin
/// ratio.
///
/// This uses floating-point math to match the TypeScript SDK logic:
///
/// 1. Convert tick bounds and current sqrt price to f64 prices
/// 2. Compute how much quote token per unit of liquidity the range covers
/// 3. Derive required liquidity from `margin / (target_ratio × quote_per_liq)`
///
/// # Arguments
///
/// - `margin_scaled`: Margin in 6-decimal units (e.g. `1_000_000` = 1 USDC)
/// - `tick_lower`, `tick_upper`: Tick range for the position
/// - `current_sqrt_price_x96`: Current pool sqrtPriceX96
/// - `target_margin_ratio`: Target ratio as a fraction (e.g. `0.1` for 10%)
///
/// # Errors
///
/// - [`ValidationError::InvalidTickRange`] if `tick_lower >= tick_upper`
/// - [`ValidationError::InvalidMargin`] if `margin_scaled` is 0
/// - [`ValidationError::InvalidLeverage`] if `target_margin_ratio` is not in `(0, 1)`
/// - [`ValidationError::Overflow`] if the result would be non-finite
pub fn liquidity_for_target_ratio(
    margin_scaled: u128,
    tick_lower: i32,
    tick_upper: i32,
    current_sqrt_price_x96: U256,
    target_margin_ratio: f64,
) -> Result<u128, ValidationError> {
    if tick_lower >= tick_upper {
        return Err(ValidationError::InvalidTickRange {
            lower: tick_lower,
            upper: tick_upper,
        });
    }
    if target_margin_ratio <= 0.0 || target_margin_ratio >= 1.0 {
        return Err(ValidationError::InvalidLeverage {
            reason: format!("target_margin_ratio must be in (0, 1), got {target_margin_ratio}"),
        });
    }
    if margin_scaled == 0 {
        return Err(ValidationError::InvalidMargin {
            reason: "margin must be non-zero".into(),
        });
    }

    // Convert sqrtPriceX96 values to f64 for the ratio calculation.
    let sqrt_lower_x96 = get_sqrt_ratio_at_tick(tick_lower)?;
    let sqrt_upper_x96 = get_sqrt_ratio_at_tick(tick_upper)?;

    let q96_f = crate::constants::Q96_U128 as f64;

    let to_f64 = |v: U256| -> Result<f64, ValidationError> {
        u128::try_from(v)
            .map(|n| n as f64)
            .map_err(|_| ValidationError::Overflow {
                context: "sqrtPriceX96 exceeds u128 range".into(),
            })
    };

    let sqrt_lower_f = to_f64(sqrt_lower_x96)? / q96_f;
    let sqrt_upper_f = to_f64(sqrt_upper_x96)? / q96_f;
    let sqrt_current_f = to_f64(current_sqrt_price_x96)? / q96_f;

    // Quote token amount per unit of liquidity depends on where current price
    // sits relative to the range.
    let quote_per_liq = if sqrt_current_f <= sqrt_lower_f {
        // Current price below range: all tokens are quote.
        sqrt_upper_f - sqrt_lower_f
    } else if sqrt_current_f >= sqrt_upper_f {
        // Current price above range: position is fully in base, no quote.
        0.0
    } else {
        // Current price inside range.
        sqrt_upper_f - sqrt_current_f
    };

    if quote_per_liq <= 0.0 {
        return Err(ValidationError::Overflow {
            context: "quote_per_liq is zero (price above range)".into(),
        });
    }

    // margin = target_margin_ratio × notional_value
    // notional_value ≈ liquidity × quote_per_liq
    // => liquidity = margin / (target_margin_ratio × quote_per_liq)
    let margin_f = margin_scaled as f64;
    let liquidity_f = margin_f / (target_margin_ratio * quote_per_liq);

    if !liquidity_f.is_finite() || liquidity_f <= 0.0 {
        return Err(ValidationError::Overflow {
            context: format!("computed liquidity is not finite: {liquidity_f}"),
        });
    }

    Ok(liquidity_f as u128)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── estimate_liquidity ───────────────────────────────────────

    #[test]
    fn estimate_liquidity_basic() {
        // Small range, 1 USDC → should get a positive liquidity value.
        let liq = estimate_liquidity(-100, 100, 1_000_000).unwrap();
        assert!(!liq.is_zero(), "liquidity should be positive");
    }

    #[test]
    fn estimate_liquidity_wider_range_gives_less_liquidity() {
        // For the same USD amount, a wider range requires less liquidity per unit
        // of price range, but the formula L = usd * Q96 / delta means wider delta
        // → lower L. Verify this inverse relationship.
        let narrow = estimate_liquidity(-100, 100, 1_000_000).unwrap();
        let wide = estimate_liquidity(-1000, 1000, 1_000_000).unwrap();
        assert!(
            narrow > wide,
            "narrower range should concentrate more liquidity: narrow={narrow}, wide={wide}"
        );
    }

    #[test]
    fn estimate_liquidity_more_usd_gives_more_liquidity() {
        let small = estimate_liquidity(-100, 100, 1_000_000).unwrap();
        let large = estimate_liquidity(-100, 100, 10_000_000).unwrap();
        assert!(
            large > small,
            "more USD should give more liquidity: large={large}, small={small}"
        );
    }

    #[test]
    fn estimate_liquidity_proportional_to_usd() {
        // Doubling USD should approximately double liquidity (linear relationship).
        // Not exactly 2× due to integer division truncation: 2*(x/d) can differ
        // from (2*x)/d by at most 1.
        let base = estimate_liquidity(-1000, 1000, 1_000_000).unwrap();
        let doubled = estimate_liquidity(-1000, 1000, 2_000_000).unwrap();
        let diff = doubled.abs_diff(base * U256::from(2u64));
        assert!(
            diff <= U256::from(1u64),
            "expected proportional within ±1, got diff={diff}"
        );
    }

    #[test]
    fn estimate_liquidity_rejects_equal_ticks() {
        assert!(estimate_liquidity(100, 100, 1_000_000).is_err());
    }

    #[test]
    fn estimate_liquidity_rejects_inverted_ticks() {
        assert!(estimate_liquidity(200, 100, 1_000_000).is_err());
    }

    #[test]
    fn estimate_liquidity_rejects_zero_amount() {
        assert!(estimate_liquidity(-100, 100, 0).is_err());
    }

    // ── liquidity_for_target_ratio ──────────────────────────────

    #[test]
    fn target_ratio_basic() {
        let liq = liquidity_for_target_ratio(
            1_000_000, // 1 USDC
            -1000, 1000, Q96, // current price = 1.0 (at tick 0)
            0.1, // 10% margin ratio
        )
        .unwrap();
        assert!(liq > 0, "liquidity should be positive");
    }

    #[test]
    fn target_ratio_higher_ratio_gives_less_liquidity() {
        // Higher margin ratio → less leveraged → less liquidity needed for same margin.
        let low_ratio = liquidity_for_target_ratio(1_000_000, -1000, 1000, Q96, 0.05).unwrap();
        let high_ratio = liquidity_for_target_ratio(1_000_000, -1000, 1000, Q96, 0.2).unwrap();
        assert!(
            low_ratio > high_ratio,
            "lower ratio needs more liquidity: low={low_ratio}, high={high_ratio}"
        );
    }

    #[test]
    fn target_ratio_more_margin_gives_more_liquidity() {
        let small = liquidity_for_target_ratio(1_000_000, -1000, 1000, Q96, 0.1).unwrap();
        let large = liquidity_for_target_ratio(10_000_000, -1000, 1000, Q96, 0.1).unwrap();
        assert!(
            large > small,
            "more margin should give more liquidity: large={large}, small={small}"
        );
    }

    #[test]
    fn target_ratio_rejects_invalid_tick_range() {
        assert!(liquidity_for_target_ratio(1_000_000, 100, 100, Q96, 0.1).is_err());
    }

    #[test]
    fn target_ratio_rejects_zero_ratio() {
        assert!(liquidity_for_target_ratio(1_000_000, -100, 100, Q96, 0.0).is_err());
    }

    #[test]
    fn target_ratio_rejects_ratio_at_one() {
        assert!(liquidity_for_target_ratio(1_000_000, -100, 100, Q96, 1.0).is_err());
    }

    #[test]
    fn target_ratio_rejects_negative_ratio() {
        assert!(liquidity_for_target_ratio(1_000_000, -100, 100, Q96, -0.1).is_err());
    }

    #[test]
    fn target_ratio_rejects_zero_margin() {
        assert!(liquidity_for_target_ratio(0, -100, 100, Q96, 0.1).is_err());
    }

    #[test]
    fn target_ratio_price_above_range() {
        // If current price is above the entire range, quote_per_liq = 0 → error.
        // Tick 2000 is well above the range [-1000, -500].
        let sqrt_above = super::super::tick::get_sqrt_ratio_at_tick(2000).unwrap();
        assert!(liquidity_for_target_ratio(1_000_000, -1000, -500, sqrt_above, 0.1).is_err());
    }
}
