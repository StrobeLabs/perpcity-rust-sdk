//! Liquidity sizing and band amounts for PerpCity maker positions.
//!
//! These functions help determine how much liquidity to provide across a
//! tick range, either from a flat USD amount or targeting a specific margin
//! ratio, and what token amounts a band's liquidity holds at a price
//! ([`amounts_for_liquidity`]).

use alloy::primitives::U256;

use crate::constants::Q96;
use crate::errors::ValidationError;
use crate::math::range::{MakerBand, TickRange};
use crate::math::swap::{amount0_delta, amount1_delta};
use crate::units::fixed_point::{Rounding, mul_div};
use crate::units::{LDelta, LUnits, PerpAtoms, PerpDelta, SqrtPrice, UsdcAtoms, UsdcDelta};

/// Estimate the liquidity needed to deploy `usd_amount` of value across
/// `range`.
///
/// Uses the Uniswap V3/V4 formula for concentrated liquidity:
///
/// ```text
/// L = (usd_amount_scaled × 2^96) / (sqrtPriceUpper − sqrtPriceLower)
/// ```
///
/// # Errors
///
/// - [`ValidationError::InvalidMargin`] if `usd` is zero
/// - [`ValidationError::Overflow`] if the sqrt price delta is zero, or if
///   the result exceeds the `uint128` the pool stores, which is a size
///   nothing could place rather than a large position
pub fn estimate_liquidity(range: &TickRange, usd: UsdcAtoms) -> Result<LUnits, ValidationError> {
    if usd.is_zero() {
        return Err(ValidationError::InvalidMargin {
            reason: "USD amount must be non-zero".into(),
        });
    }

    let (sqrt_lower, sqrt_upper) = range.sqrt_bounds();

    let delta = sqrt_upper.x96() - sqrt_lower.x96();
    if delta.is_zero() {
        return Err(ValidationError::Overflow {
            context: "sqrtPrice delta is zero".into(),
        });
    }

    let numerator = U256::from(usd.atoms()) * Q96;
    LUnits::try_from(numerator / delta)
}

/// The margin `liquidity` over `range` requires: the inverse of
/// [`estimate_liquidity`], `L · (√P_hi − √P_lo) / 2^96`, rounded up so
/// that `estimate_liquidity(margin_for_liquidity(L))` is at least `L`.
///
/// # Errors
///
/// [`ValidationError::Overflow`] if the figure exceeds the width the
/// contracts hold a balance in.
pub fn margin_for_liquidity(
    range: &TickRange,
    liquidity: LUnits,
) -> Result<UsdcAtoms, ValidationError> {
    let (sqrt_lower, sqrt_upper) = range.sqrt_bounds();
    let atoms = mul_div(
        U256::from(liquidity.units()),
        sqrt_upper.x96() - sqrt_lower.x96(),
        Q96,
        Rounding::Up,
    )?;
    UsdcAtoms::try_from(atoms)
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
/// - `margin`: the margin backing the position
/// - `range`: Tick range for the position
/// - `current_sqrt_price`: the pool's current price
/// - `target_margin_ratio`: Target ratio as a fraction (e.g. `0.1` for 10%)
///
/// # Errors
///
/// - [`ValidationError::InvalidMargin`] if `margin` is zero
/// - [`ValidationError::InvalidLeverage`] if `target_margin_ratio` is not in `(0, 1)`
/// - [`ValidationError::Overflow`] if the result would be non-finite
pub fn liquidity_for_target_ratio(
    margin: UsdcAtoms,
    range: &TickRange,
    current_sqrt_price: SqrtPrice,
    target_margin_ratio: f64,
) -> Result<LUnits, ValidationError> {
    if target_margin_ratio <= 0.0 || target_margin_ratio >= 1.0 {
        return Err(ValidationError::InvalidLeverage {
            reason: format!("target_margin_ratio must be in (0, 1), got {target_margin_ratio}"),
        });
    }
    if margin.is_zero() {
        return Err(ValidationError::InvalidMargin {
            reason: "margin must be non-zero".into(),
        });
    }

    // Convert sqrtPriceX96 values to f64 for the ratio calculation.
    let (sqrt_lower, sqrt_upper) = range.sqrt_bounds();

    let q96_f = crate::constants::Q96_U128 as f64;

    let to_f64 = |v: SqrtPrice| -> Result<f64, ValidationError> {
        u128::try_from(v.x96())
            .map(|n| n as f64)
            .map_err(|_| ValidationError::Overflow {
                context: "sqrtPriceX96 exceeds u128 range".into(),
            })
    };

    let sqrt_lower_f = to_f64(sqrt_lower)? / q96_f;
    let sqrt_upper_f = to_f64(sqrt_upper)? / q96_f;
    let sqrt_current_f = to_f64(current_sqrt_price)? / q96_f;

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
    let margin_f = margin.atoms() as f64;
    let liquidity_f = margin_f / (target_margin_ratio * quote_per_liq);

    if !liquidity_f.is_finite() || liquidity_f <= 0.0 {
        return Err(ValidationError::Overflow {
            context: format!("computed liquidity is not finite: {liquidity_f}"),
        });
    }
    // A float-to-integer cast saturates rather than wrapping, so without
    // this a size past the pool's `uint128` would silently become the
    // largest one — the same failure `estimate_liquidity` refuses.
    if liquidity_f >= u128::MAX as f64 {
        return Err(ValidationError::Overflow {
            context: "liquidity exceeds the uint128 the pool stores".into(),
        });
    }

    Ok(LUnits::new(liquidity_f as u128))
}

/// Uniswap `LiquidityAmounts.getAmountsForLiquidity`: what `liquidity`
/// holds across the band `[sqrt_price_a, sqrt_price_b]` at `sqrt_price`,
/// both rounded down.
///
/// In a PerpCity pool currency0 is the perp token and currency1 is USDC, so
/// the two legs are the two assets and come back as their own types. The
/// bounds may come in either order. The price is clamped into the band: at
/// or below it the band holds only perps, at or above it only USDC.
///
/// # Examples
///
/// The perps a band holds at a price. It equals the band's long capacity
/// at that price:
///
/// ```
/// use perpcity_sdk::math::liquidity::band_amounts;
/// use perpcity_sdk::{LUnits, MakerBand, Price, SqrtPrice, TickRange, band_capacity};
///
/// let sqrt_price = SqrtPrice::try_from(Price::try_from(35.0)?)?;
/// let band = MakerBand::new(TickRange::new(27_090, 38_100)?, LUnits::new(1_757_959));
/// let (perp, _usdc) = band_amounts(sqrt_price, &band)?;
/// assert_eq!(perp, band_capacity(sqrt_price, &band)?.long);
/// # Ok::<(), perpcity_sdk::ValidationError>(())
/// ```
///
/// # Errors
///
/// - [`ValidationError::InvalidPrice`] if any sqrt price is zero
/// - [`ValidationError::Overflow`] if an amount exceeds the width the
///   contracts hold one in
pub fn amounts_for_liquidity(
    sqrt_price: SqrtPrice,
    sqrt_price_a: SqrtPrice,
    sqrt_price_b: SqrtPrice,
    liquidity: LUnits,
) -> Result<(PerpAtoms, UsdcAtoms), ValidationError> {
    if sqrt_price.is_zero() || sqrt_price_a.is_zero() || sqrt_price_b.is_zero() {
        return Err(ValidationError::InvalidPrice {
            reason: "zero sqrt price".into(),
        });
    }
    let (sa, sb) = if sqrt_price_a <= sqrt_price_b {
        (sqrt_price_a.x96(), sqrt_price_b.x96())
    } else {
        (sqrt_price_b.x96(), sqrt_price_a.x96())
    };
    let sp = sqrt_price.x96().clamp(sa, sb);
    let l = liquidity.units();
    let amount0 = amount0_delta(sp, sb, l, Rounding::TowardZero)?;
    let amount1 = amount1_delta(sa, sp, l, Rounding::TowardZero)?;
    Ok((
        PerpAtoms::new(narrow(amount0, "band perp amount")?),
        UsdcAtoms::new(narrow(amount1, "band USDC amount")?),
    ))
}

/// A band leg narrowed to the width the contracts hold an amount in.
fn narrow(amount: U256, what: &str) -> Result<u128, ValidationError> {
    u128::try_from(amount).map_err(|_| ValidationError::Overflow {
        context: format!("{what} {amount} exceeds uint128"),
    })
}

/// Uniswap `Pool.modifyLiquidity`'s principal: what changing `range`'s
/// liquidity by `delta` moves, as the caller's two legs, the position's
/// `delta` in the contract. An add pays both legs in, each rounded up, and
/// comes back negative; a removal takes them out, rounded down, and comes
/// back positive. So a band added and then removed whole is left a few
/// atoms short.
///
/// Which legs move is the pool's `tick` against the bounds, as V4 decides
/// it, and the amounts are taken at `sqrt_price`: both are the pool's own,
/// from its last swap or its creation.
///
/// # Examples
///
/// A band added and removed at the same price comes back an atom short of
/// each leg it held:
///
/// ```
/// use perpcity_sdk::{LDelta, TickRange, get_sqrt_ratio_at_tick, liquidity_change_delta};
///
/// let range = TickRange::new(38_670, 38_760)?;
/// let sqrt_price = get_sqrt_ratio_at_tick(38_692)?;
/// let (perp_in, usd_in) = liquidity_change_delta(sqrt_price, 38_692, &range, LDelta::new(317_573_271))?;
/// let (perp_out, usd_out) = liquidity_change_delta(sqrt_price, 38_692, &range, LDelta::new(-317_573_271))?;
/// assert_eq!((perp_in + perp_out).atoms(), -1);
/// assert_eq!((usd_in + usd_out).atoms(), -1);
/// # Ok::<(), perpcity_sdk::ValidationError>(())
/// ```
///
/// # Errors
///
/// - [`ValidationError::InvalidPrice`] if `sqrt_price` is zero
/// - [`ValidationError::Overflow`] if a leg exceeds the `int128` a balance
///   delta holds
pub fn liquidity_change_delta(
    sqrt_price: SqrtPrice,
    tick: i32,
    range: &TickRange,
    delta: LDelta,
) -> Result<(PerpDelta, UsdcDelta), ValidationError> {
    if sqrt_price.is_zero() {
        return Err(ValidationError::InvalidPrice {
            reason: "zero sqrt price".into(),
        });
    }
    let (sqrt_lower, sqrt_upper) = range.sqrt_bounds();
    let (sa, sb, sp) = (sqrt_lower.x96(), sqrt_upper.x96(), sqrt_price.x96());
    let (perp, usd) = if tick < range.lower() {
        (amount0_change(sa, sb, delta)?, 0)
    } else if tick < range.upper() {
        (
            amount0_change(sp, sb, delta)?,
            amount1_change(sa, sp, delta)?,
        )
    } else {
        (0, amount1_change(sa, sb, delta)?)
    };
    Ok((PerpDelta::new(perp), UsdcDelta::new(usd)))
}

/// `SqrtPriceMath.getAmount0Delta` over a signed liquidity change, from the
/// caller's side: paid in (negative, rounded up) on an add, taken out
/// (positive, rounded down) on a removal.
fn amount0_change(a: U256, b: U256, delta: LDelta) -> Result<i128, ValidationError> {
    let (units, rounding) = change_rounding(delta);
    signed_leg(amount0_delta(a, b, units, rounding)?, delta, "perp leg")
}

/// `SqrtPriceMath.getAmount1Delta`, signed as [`amount0_change`].
fn amount1_change(a: U256, b: U256, delta: LDelta) -> Result<i128, ValidationError> {
    let (units, rounding) = change_rounding(delta);
    signed_leg(amount1_delta(a, b, units, rounding)?, delta, "USDC leg")
}

/// The change's size, and the rounding that leaves the pool never short.
fn change_rounding(delta: LDelta) -> (u128, Rounding) {
    let units = delta.units().unsigned_abs();
    if delta.units() < 0 {
        (units, Rounding::TowardZero)
    } else {
        (units, Rounding::Up)
    }
}

/// An amount signed from the caller's side: taken out on a removal, paid
/// in on an add.
fn signed_leg(amount: U256, delta: LDelta, what: &str) -> Result<i128, ValidationError> {
    let amount = i128::try_from(amount).map_err(|_| ValidationError::Overflow {
        context: format!("{what} {amount} exceeds int128"),
    })?;
    Ok(if delta.units() < 0 { amount } else { -amount })
}

/// What `band` holds at `sqrt_price`: [`amounts_for_liquidity`] over the
/// band's own bounds.
///
/// # Errors
///
/// As [`amounts_for_liquidity`].
pub fn band_amounts(
    sqrt_price: SqrtPrice,
    band: &MakerBand,
) -> Result<(PerpAtoms, UsdcAtoms), ValidationError> {
    let (sqrt_lower, sqrt_upper) = band.range.sqrt_bounds();
    amounts_for_liquidity(sqrt_price, sqrt_lower, sqrt_upper, band.liquidity)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Three of one Arbitrum band's changes (NYCRT v0.2.2, position 29,
    /// range 38670..38760), each at the pool's price and tick then: an add
    /// in range, an add with the price below the band, a removal in range.
    /// The amounts are what the contract added to the position's delta.
    #[test]
    fn a_liquidity_change_moves_what_the_pool_moved() {
        let range = TickRange::new(38_670, 38_760).unwrap();
        let cases = [
            (
                "548325480008142788545567093195",
                38_692,
                317_573_271,
                (-154_189, -2_490_753),
            ),
            // Below the band the price does not enter.
            ("1", 37_719, 797_278_127, (-517_795, 0)),
            (
                "549974192257810920237436044902",
                38_752,
                -316_422_905,
                (16_569, 9_066_387),
            ),
        ];
        for (sqrt, tick, delta, (perp, usd)) in cases {
            let sqrt = SqrtPrice::from_x96(sqrt.parse().unwrap());
            let moved = liquidity_change_delta(sqrt, tick, &range, LDelta::new(delta)).unwrap();
            assert_eq!(
                moved,
                (PerpDelta::new(perp), UsdcDelta::new(usd)),
                "{delta} at tick {tick}"
            );
        }
    }

    /// The legs follow the tick, not the price: at a bound's tick the band
    /// is in range on the lower bound and out of it on the upper.
    #[test]
    fn the_tick_decides_which_legs_move() {
        let range = TickRange::new(38_670, 38_760).unwrap();
        let (lower, upper) = range.sqrt_bounds();
        let add = LDelta::new(1_000_000_000);
        let at_lower = liquidity_change_delta(lower, 38_670, &range, add).unwrap();
        assert!(at_lower.0.is_negative() && at_lower.1.is_zero());
        let at_upper = liquidity_change_delta(upper, 38_760, &range, add).unwrap();
        assert!(at_upper.0.is_zero() && at_upper.1.is_negative());
    }

    /// The margin a depth requires is the inverse of sizing a depth from a
    /// margin, rounded so the round trip never comes back short.
    #[test]
    fn the_margin_for_a_depth_inverts_the_sizing() {
        let range = TickRange::new(46_020, 46_080).unwrap();
        for units in [1u128, 1_000, 1_234_567_891_011, 1 << 100] {
            let depth = LUnits::new(units);
            let margin = margin_for_liquidity(&range, depth).unwrap();
            let back = estimate_liquidity(&range, margin).unwrap();
            assert!(back >= depth, "{units}: {margin:?} sizes only {back:?}");
            // And one atom less does not reach it.
            if margin.atoms() > 1 {
                let less = UsdcAtoms::new(margin.atoms() - 1);
                assert!(estimate_liquidity(&range, less).unwrap() < depth, "{units}");
            }
        }
    }
    use crate::math::tick::get_sqrt_ratio_at_tick;

    fn range(lower: i32, upper: i32) -> TickRange {
        TickRange::new(lower, upper).unwrap()
    }

    /// A USDC amount in atoms.
    fn usd(atoms: u128) -> UsdcAtoms {
        UsdcAtoms::new(atoms)
    }

    /// A pool price of one, which is tick zero.
    fn one() -> SqrtPrice {
        SqrtPrice::from_x96(Q96)
    }

    // ── estimate_liquidity ───────────────────────────────────────

    #[test]
    fn estimate_liquidity_basic() {
        // Small range, 1 USDC → should get a positive liquidity value.
        let liq = estimate_liquidity(&range(-100, 100), usd(1_000_000)).unwrap();
        assert!(!liq.is_zero(), "liquidity should be positive");
    }

    #[test]
    fn estimate_liquidity_wider_range_gives_less_liquidity() {
        // For the same USD amount, a wider range requires less liquidity per unit
        // of price range, but the formula L = usd * Q96 / delta means wider delta
        // → lower L. Verify this inverse relationship.
        let narrow = estimate_liquidity(&range(-100, 100), usd(1_000_000)).unwrap();
        let wide = estimate_liquidity(&range(-1000, 1000), usd(1_000_000)).unwrap();
        assert!(
            narrow > wide,
            "narrower range should concentrate more liquidity: narrow={narrow:?}, wide={wide:?}"
        );
    }

    #[test]
    fn estimate_liquidity_more_usd_gives_more_liquidity() {
        let small = estimate_liquidity(&range(-100, 100), usd(1_000_000)).unwrap();
        let large = estimate_liquidity(&range(-100, 100), usd(10_000_000)).unwrap();
        assert!(
            large > small,
            "more USD should give more liquidity: large={large:?}, small={small:?}"
        );
    }

    #[test]
    fn estimate_liquidity_proportional_to_usd() {
        // Doubling USD should approximately double liquidity (linear relationship).
        // Not exactly 2× due to integer division truncation: 2*(x/d) can differ
        // from (2*x)/d by at most 1.
        let base = estimate_liquidity(&range(-1000, 1000), usd(1_000_000)).unwrap();
        let doubled = estimate_liquidity(&range(-1000, 1000), usd(2_000_000)).unwrap();
        let diff = doubled.units().abs_diff(base.units() * 2);
        assert!(
            diff <= 1,
            "expected proportional within ±1, got diff={diff}"
        );
    }

    #[test]
    fn estimate_liquidity_rejects_zero_amount() {
        assert!(estimate_liquidity(&range(-100, 100), UsdcAtoms::ZERO).is_err());
    }

    // ── liquidity_for_target_ratio ──────────────────────────────

    #[test]
    fn target_ratio_basic() {
        let liq = liquidity_for_target_ratio(
            usd(1_000_000), // 1 USDC
            &range(-1000, 1000),
            one(), // current price = 1.0 (at tick 0)
            0.1,   // 10% margin ratio
        )
        .unwrap();
        assert!(!liq.is_zero(), "liquidity should be positive");
    }

    #[test]
    fn target_ratio_higher_ratio_gives_less_liquidity() {
        // Higher margin ratio → less leveraged → less liquidity needed for same margin.
        let low_ratio =
            liquidity_for_target_ratio(usd(1_000_000), &range(-1000, 1000), one(), 0.05).unwrap();
        let high_ratio =
            liquidity_for_target_ratio(usd(1_000_000), &range(-1000, 1000), one(), 0.2).unwrap();
        assert!(
            low_ratio > high_ratio,
            "lower ratio needs more liquidity: low={low_ratio:?}, high={high_ratio:?}"
        );
    }

    #[test]
    fn target_ratio_more_margin_gives_more_liquidity() {
        let small =
            liquidity_for_target_ratio(usd(1_000_000), &range(-1000, 1000), one(), 0.1).unwrap();
        let large =
            liquidity_for_target_ratio(usd(10_000_000), &range(-1000, 1000), one(), 0.1).unwrap();
        assert!(
            large > small,
            "more margin should give more liquidity: large={large:?}, small={small:?}"
        );
    }

    #[test]
    fn target_ratio_rejects_a_ratio_outside_the_open_unit_interval() {
        for ratio in [0.0, 1.0, -0.1] {
            assert!(
                liquidity_for_target_ratio(usd(1_000_000), &range(-100, 100), one(), ratio)
                    .is_err(),
                "ratio {ratio}"
            );
        }
    }

    #[test]
    fn target_ratio_rejects_zero_margin() {
        assert!(
            liquidity_for_target_ratio(UsdcAtoms::ZERO, &range(-100, 100), one(), 0.1).is_err()
        );
    }

    /// A size past the `uint128` the pool stores fails rather than becoming
    /// the largest one: a float-to-integer cast saturates, so the bound has
    /// to be checked before it.
    #[test]
    fn target_ratio_rejects_a_size_past_the_pools_width() {
        // A vanishingly thin band over an enormous margin: the liquidity per
        // unit of quote is tiny, so the required liquidity leaves `u128`.
        let huge = UsdcAtoms::new(u128::MAX / 2);
        let err = liquidity_for_target_ratio(huge, &range(-30, 30), one(), 1e-6).unwrap_err();
        assert!(matches!(err, ValidationError::Overflow { .. }), "{err}");
    }

    // ── amounts_for_liquidity ────────────────────────────────────

    /// Uniswap's three branches, with the price at each band edge landing
    /// on the one-sided result.
    #[test]
    fn amounts_follow_price_position() {
        let sa = get_sqrt_ratio_at_tick(-600).unwrap();
        let sb = get_sqrt_ratio_at_tick(600).unwrap();
        let liquidity = LUnits::new(1_000_000_000);
        let whole = |amount: U256| amount.to::<u128>();
        let full0 = PerpAtoms::new(whole(
            amount0_delta(sa.x96(), sb.x96(), liquidity.units(), Rounding::TowardZero).unwrap(),
        ));
        let full1 = UsdcAtoms::new(whole(
            amount1_delta(sa.x96(), sb.x96(), liquidity.units(), Rounding::TowardZero).unwrap(),
        ));

        let below = get_sqrt_ratio_at_tick(-1200).unwrap();
        let above = get_sqrt_ratio_at_tick(1200).unwrap();
        assert_eq!(
            amounts_for_liquidity(below, sa, sb, liquidity).unwrap(),
            (full0, UsdcAtoms::ZERO)
        );
        assert_eq!(
            amounts_for_liquidity(sa, sa, sb, liquidity).unwrap(),
            (full0, UsdcAtoms::ZERO)
        );
        assert_eq!(
            amounts_for_liquidity(above, sa, sb, liquidity).unwrap(),
            (PerpAtoms::ZERO, full1)
        );
        assert_eq!(
            amounts_for_liquidity(sb, sa, sb, liquidity).unwrap(),
            (PerpAtoms::ZERO, full1)
        );

        let (in0, in1) = amounts_for_liquidity(one(), sa, sb, liquidity).unwrap();
        assert!(!in0.is_zero() && in0 < full0);
        assert!(!in1.is_zero() && in1 < full1);
    }

    #[test]
    fn amounts_accept_bounds_in_either_order() {
        let sa = get_sqrt_ratio_at_tick(-600).unwrap();
        let sb = get_sqrt_ratio_at_tick(600).unwrap();
        assert_eq!(
            amounts_for_liquidity(one(), sa, sb, LUnits::new(1_000_000)).unwrap(),
            amounts_for_liquidity(one(), sb, sa, LUnits::new(1_000_000)).unwrap()
        );
    }

    #[test]
    fn amounts_reject_zero_prices() {
        let sb = get_sqrt_ratio_at_tick(600).unwrap();
        let zero = SqrtPrice::from_x96(U256::ZERO);
        assert!(matches!(
            amounts_for_liquidity(zero, one(), sb, LUnits::new(1)),
            Err(ValidationError::InvalidPrice { .. })
        ));
        assert!(matches!(
            amounts_for_liquidity(one(), zero, sb, LUnits::new(1)),
            Err(ValidationError::InvalidPrice { .. })
        ));
    }

    #[test]
    fn target_ratio_price_above_range() {
        // If current price is above the entire range, quote_per_liq = 0 → error.
        // Tick 2000 is well above the range [-1000, -500].
        let sqrt_above = get_sqrt_ratio_at_tick(2000).unwrap();
        assert!(
            liquidity_for_target_ratio(usd(1_000_000), &range(-1000, -500), sqrt_above, 0.1)
                .is_err()
        );
    }
}
