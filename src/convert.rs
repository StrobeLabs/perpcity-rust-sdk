//! The `f64` doors onto the units, kept for the event decoder.
//!
//! Every function here is one line over [`crate::units`], which owns the
//! arithmetic: a scaling is a conversion on an amount type, a price
//! conversion is a constructor or accessor on [`Price`] or [`SqrtPrice`].
//! They remain because the event vocabulary is human units by design and
//! decodes through them; they go when the decoder speaks the units
//! directly, which leaves only the V4 balance-delta packing in this module.
//!
//! All of them validate and return [`Result`] on failure.
//!
//! # Precision model
//!
//! USDC has 6 decimals, so `1.0 USDC` = `1_000_000` on-chain. The
//! [`scale_to_6dec`] / [`scale_from_6dec`] pair handles this scaling.
//!
//! Uniswap V4 prices are stored as `sqrtPriceX96 = sqrt(price) × 2^96`.
//! The [`price_to_sqrt_price_x96`] / [`sqrt_price_x96_to_price`] pair
//! handles this encoding, using a 6-decimal intermediate for precision.

use std::fmt;

use alloy::primitives::{I256, U256};

use crate::errors::ValidationError;
use crate::units::{Price, Ratio, SqrtPrice, UsdcDelta};

// ── Scaling: f64 ↔ 6-decimal integers ──────────────────────────────────

/// Scale a human-readable amount to its 6-decimal on-chain representation.
///
/// Supports negative values (for `marginDelta`, `usdDelta`, etc.).
/// Rounds to the nearest atom, as [`UsdcDelta::try_from`] does: the
/// amount is a number a person wrote, not a contract word.
///
/// # Errors
///
/// Returns [`ValidationError::Overflow`] if `|amount|` exceeds the safe
/// f64 integer range (2^53).
///
/// # Examples
///
/// ```
/// # use perpcity_sdk::convert::scale_to_6dec;
/// assert_eq!(scale_to_6dec(1.5).unwrap(), 1_500_000);
/// assert_eq!(scale_to_6dec(-2.5).unwrap(), -2_500_000);
/// ```
pub fn scale_to_6dec(amount: f64) -> Result<i128, ValidationError> {
    UsdcDelta::try_from(amount).map(UsdcDelta::atoms)
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
/// # use perpcity_sdk::convert::scale_from_6dec;
/// assert_eq!(scale_from_6dec(1_500_000), 1.5);
/// assert_eq!(scale_from_6dec(-2_000_000), -2.0);
/// ```
pub fn scale_from_6dec(value: i128) -> f64 {
    UsdcDelta::new(value).usdc()
}

/// A 6-decimal on-chain amount as human-readable f64, for the unsigned
/// widths the contracts return (`uint128`, `uint256`).
///
/// No balance or margin comes near `i128`, so a value past it is a broken
/// read rather than a quantity; `what` names the figure in the error.
///
/// # Errors
///
/// [`ValidationError::Overflow`] if `atoms` does not fit `i128`.
///
/// # Examples
///
/// ```
/// # use perpcity_sdk::convert::usdc_from_atoms;
/// assert_eq!(usdc_from_atoms(1_500_000u128, "margin")?, 1.5);
/// assert!(usdc_from_atoms(u128::MAX, "margin").is_err());
/// # Ok::<(), perpcity_sdk::ValidationError>(())
/// ```
pub fn usdc_from_atoms<A>(atoms: A, what: &str) -> Result<f64, ValidationError>
where
    A: TryInto<i128> + fmt::Display + Copy,
{
    let atoms = atoms.try_into().map_err(|_| ValidationError::Overflow {
        context: format!("{what} {atoms} exceeds i128"),
    })?;
    Ok(scale_from_6dec(atoms))
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
/// Returns [`ValidationError::InvalidLeverage`] if leverage is zero,
/// negative, NaN, or produces an out-of-range margin ratio.
///
/// # Examples
///
/// ```
/// # use perpcity_sdk::convert::leverage_to_margin_ratio;
/// assert_eq!(leverage_to_margin_ratio(10.0)?.e6(), 100_000);
/// assert_eq!(leverage_to_margin_ratio(1.0)?.e6(), 1_000_000);
/// assert_eq!(leverage_to_margin_ratio(100.0)?.e6(), 10_000);
/// # Ok::<(), perpcity_sdk::ValidationError>(())
/// ```
pub fn leverage_to_margin_ratio(leverage: f64) -> Result<Ratio, ValidationError> {
    if !leverage.is_finite() || leverage <= 0.0 {
        return Err(ValidationError::InvalidLeverage {
            reason: format!("leverage must be a positive finite number, got {leverage}"),
        });
    }
    Ratio::for_leverage(leverage)
}

/// Convert an on-chain margin ratio (scaled by 1e6) to leverage.
///
/// On-chain: `leverage = 1_000_000 / marginRatio`.
///
/// # Errors
///
/// Returns [`ValidationError::InvalidMarginRatio`] if `margin_ratio` is zero.
///
/// # Examples
///
/// ```
/// # use perpcity_sdk::convert::margin_ratio_to_leverage;
/// # use perpcity_sdk::Ratio;
/// let lev = margin_ratio_to_leverage(Ratio::from_e6(100_000)?)?;
/// assert!((lev - 10.0).abs() < 0.0001);
/// # Ok::<(), perpcity_sdk::ValidationError>(())
/// ```
pub fn margin_ratio_to_leverage(margin_ratio: Ratio) -> Result<f64, ValidationError> {
    margin_ratio.leverage()
}

// ── Q96 fixed-point ↔ f64 ─────────────────────────────────────────────

/// Convert a Q96 fixed-point value to f64.
///
/// This is the base decoder for all Q96-encoded values. The input is
/// already a price (or index), not a sqrt price.
///
/// Formula: `price = value × 1e6 / 2^96 / 1e6`
///
/// Used for beacon index values (`IndexUpdated.index`) and as the
/// building block for [`sqrt_price_x96_to_price`].
///
/// # Errors
///
/// Returns [`ValidationError::InvalidPrice`] if `value` is zero, or
/// [`ValidationError::Overflow`] if the result exceeds safe f64 range.
///
/// # Examples
///
/// ```
/// # use perpcity_sdk::convert::price_x96_to_f64;
/// # use perpcity_sdk::constants::{Q96, Q96_PRECISION};
/// // Q96 encodes price = 1.0
/// let price = price_x96_to_f64(Q96).unwrap();
/// assert!((price - 1.0).abs() < Q96_PRECISION);
/// ```
pub fn price_x96_to_f64(value: U256) -> Result<f64, ValidationError> {
    Price::from_x96(value).to_f64()
}

/// Convert a human-readable price to its Q96 fixed-point representation.
///
/// Inverse of [`price_x96_to_f64`]. A two-step conversion keeps the full
/// f64 mantissa: `price × 2^48` fits in `u128` for any accepted price, and
/// the remaining `2^48` factor is an exact shift.
///
/// # Errors
///
/// Returns [`ValidationError::InvalidPrice`] if `price` is zero, negative,
/// NaN, infinite, or at least `2^80` (where `price × 2^48` would exceed
/// `u128`).
///
/// # Examples
///
/// ```
/// # use perpcity_sdk::convert::{price_f64_to_x96, price_x96_to_f64};
/// let x96 = price_f64_to_x96(1.5).unwrap();
/// assert!((price_x96_to_f64(x96).unwrap() - 1.5).abs() < 1e-9);
/// assert!(price_f64_to_x96(0.0).is_err());
/// assert!(price_f64_to_x96(f64::NAN).is_err());
/// ```
pub fn price_f64_to_x96(price: f64) -> Result<U256, ValidationError> {
    Price::try_from(price).map(Price::x96)
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
/// Returns [`ValidationError::InvalidPrice`] if `price` is zero, negative,
/// or too large (> 1e30).
///
/// # Examples
///
/// ```
/// # use perpcity_sdk::convert::price_to_sqrt_price_x96;
/// # use perpcity_sdk::constants::Q96;
/// # use alloy::primitives::U256;
/// let result = price_to_sqrt_price_x96(1.0).unwrap();
/// // For price=1.0, sqrtPriceX96 ≈ Q96
/// let diff = result.abs_diff(Q96);
/// assert!(diff < Q96 / U256::from(1_000_000));
/// ```
pub fn price_to_sqrt_price_x96(price: f64) -> Result<U256, ValidationError> {
    SqrtPrice::from_price(price).map(SqrtPrice::x96)
}

/// Convert a `sqrtPriceX96` value back to a human-readable price.
///
/// Squares the input to get a Q96 price, then delegates to
/// [`price_x96_to_f64`].
///
/// Formula: `price = sqrtPriceX96² / 2^96` → [`price_x96_to_f64`]
///
/// # Errors
///
/// Returns [`ValidationError::InvalidPrice`] if `sqrt_price_x96` is zero,
/// or [`ValidationError::Overflow`] if the squared value overflows `U256`
/// or the result exceeds safe f64 range.
///
/// # Examples
///
/// ```
/// # use perpcity_sdk::convert::sqrt_price_x96_to_price;
/// # use perpcity_sdk::constants::{Q96, Q96_PRECISION};
/// let price = sqrt_price_x96_to_price(Q96).unwrap();
/// assert!((price - 1.0).abs() < Q96_PRECISION);
/// ```
pub fn sqrt_price_x96_to_price(sqrt_price_x96: U256) -> Result<f64, ValidationError> {
    SqrtPrice::from_x96(sqrt_price_x96).price()
}

// ── BalanceDelta unpacking ─────────────────────────────────────────────

/// Unpack a Uniswap V4 `BalanceDelta` (packed `int256`) into `(amount0,
/// amount1)` = (perp, USD), each a signed `int128` in two's-complement.
///
/// The packing is part of the contract ABI: the upper 128 bits hold
/// `amount0` and the lower 128 bits hold `amount1`. The unpacking is
/// lossless: each half is exactly 128 bits wide on-chain (`int128`), so
/// reinterpreting the shifted/masked halves as two's-complement `i128`
/// reproduces the values the contract packed — no truncation and no
/// sign-extension ambiguity is possible.
///
/// # Examples
///
/// ```
/// use alloy::primitives::I256;
/// use perpcity_sdk::convert::unpack_balance_delta;
///
/// // amount0 = -2 (upper 128 bits), amount1 = 3 (lower 128 bits).
/// let packed = (I256::try_from(-2).unwrap() << 128)
///     | I256::try_from(3u8).unwrap();
/// assert_eq!(unpack_balance_delta(packed), (-2, 3));
/// ```
pub fn unpack_balance_delta(delta: I256) -> (i128, i128) {
    let raw = delta.into_raw();
    // The shift/mask leave at most 128 significant bits, so the narrowing
    // cannot truncate; `as i128` reinterprets the two's-complement halves.
    let amount0 = (raw >> 128usize).to::<u128>() as i128;
    let amount1 = (raw & U256::from(u128::MAX)).to::<u128>() as i128;
    (amount0, amount1)
}

/// Pack two `int128` amounts into a V4 `BalanceDelta`: the inverse of
/// [`unpack_balance_delta`], for building contract structs in tests.
#[cfg(test)]
pub(crate) fn pack_balance_delta(amount0: i128, amount1: i128) -> I256 {
    let mut bytes = [0u8; 32];
    bytes[0..16].copy_from_slice(&amount0.to_be_bytes());
    bytes[16..32].copy_from_slice(&amount1.to_be_bytes());
    I256::from_be_bytes(bytes)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The V4 packing: one `int256` word holding two `int128` amounts,
    /// amount0 in the high half and amount1 in the low. The signs are
    /// independent, which is the whole reason it is not two fields — a swap
    /// pays one asset and receives the other, so the two halves of a real
    /// delta disagree.
    ///
    /// This is the one piece of arithmetic `convert` owns; the rest of the
    /// module is a line each over a unit type, and those types test
    /// themselves.
    #[test]
    fn a_balance_delta_packs_two_signed_halves() {
        for (perp, usd) in [
            (0i128, 0i128),
            (1, -1),
            (-1, 1),
            // A taker going long: perp received, USDC paid.
            (100_000_000, -100_000_000),
            // The widths, both ends, both signs.
            (i128::MAX, i128::MIN),
            (i128::MIN, i128::MAX),
        ] {
            let packed = pack_balance_delta(perp, usd);
            assert_eq!(
                unpack_balance_delta(packed),
                (perp, usd),
                "({perp}, {usd}) did not survive the round trip"
            );
        }
    }

    /// The low half does not borrow from the high one. A negative amount1 is
    /// all ones in its own 128 bits, and reading amount0 must not see them —
    /// the bug this shape invites is a sign bleeding across the halves.
    #[test]
    fn a_negative_low_half_does_not_reach_the_high_one() {
        let packed = pack_balance_delta(5, -1);
        assert_eq!(unpack_balance_delta(packed), (5, -1));
        let packed = pack_balance_delta(-1, 5);
        assert_eq!(unpack_balance_delta(packed), (-1, 5));
    }

    /// The narrowing door every balance read goes through: it takes whatever
    /// width the chain returned, and refuses rather than wrapping when the
    /// value is past what the contracts store an amount in. The message
    /// names the field, because a bare overflow says nothing about which
    /// read produced it.
    #[test]
    fn a_balance_too_wide_to_hold_is_refused_by_name() {
        assert_eq!(usdc_from_atoms(1_500_000u128, "margin").unwrap(), 1.5);
        assert_eq!(
            usdc_from_atoms(U256::from(5_000_000u64), "margin").unwrap(),
            5.0
        );
        assert_eq!(usdc_from_atoms(0u128, "margin").unwrap(), 0.0);

        let err = usdc_from_atoms(U256::MAX, "total margin").unwrap_err();
        assert!(
            err.to_string().contains("total margin"),
            "the refusal should name the field: {err}"
        );
    }
}
