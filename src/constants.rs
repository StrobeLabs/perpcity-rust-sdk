//! Protocol constants from `perpcity-contracts/src/libraries/Constants.sol`.
//!
//! All values are exact mirrors of the on-chain constants. Scaling factors
//! use the same names as the Solidity source to eliminate any ambiguity.

use alloy::primitives::{U256, uint};

/// 2^96 as `U256` — the fixed-point denominator for sqrtPriceX96 values.
pub const Q96: U256 = U256::from_limbs([0, 0x1_0000_0000, 0, 0]);

/// 2^96 as `u128` for intermediate math that doesn't need full U256.
pub const Q96_U128: u128 = 1_u128 << 96;

/// 10^6 — the scaling factor for USDC amounts, margin ratios, and fees.
pub const SCALE_1E6: u32 = 1_000_000;

/// 0.5 scaled by 1e6 (i.e. 500_000).
pub const ONE_HALF: u32 = 500_000;

/// 10^18 — WAD scaling factor used in some internal math.
pub const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

/// 1% scaled by WAD (10^16).
pub const WAD_ONE_PERCENT: U256 = U256::from_limbs([10_000_000_000_000_000, 0, 0, 0]);

/// Tick spacing for all PerpCity pools.
pub const TICK_SPACING: i32 = 30;

/// Time-weighted average price window in seconds (1 hour).
pub const TWAVG_WINDOW: u32 = 3600;

/// Minimum margin required to open a position: 5 USDC (5 * 10^6).
pub const MIN_OPENING_MARGIN: u32 = 5_000_000;

/// Funding interval in seconds (1 day).
pub const INTERVAL: u64 = 86_400;

/// sqrt(0.001) * 2^96 — the minimum allowed sqrtPriceX96.
pub const MIN_SQRT_PRICE_X96: U256 = uint!(2505414483750479311864138016_U256);

/// sqrt(1000) * 2^96 — the maximum allowed sqrtPriceX96.
pub const MAX_SQRT_PRICE_X96: U256 = uint!(2505414483750479311864138015696_U256);

/// Minimum tick (~= TickMath.getTickAtSqrtPrice(MIN_SQRT_PRICE_X96)).
pub const MIN_TICK: i32 = -69_090;

/// Maximum tick (~= TickMath.getTickAtSqrtPrice(MAX_SQRT_PRICE_X96)).
pub const MAX_TICK: i32 = 69_090;

/// Total supply of the internal accounting token: type(uint120).max.
pub const ACCOUNTING_TOKEN_SUPPLY: U256 = U256::from_limbs([u64::MAX, u64::MAX >> 8, 0, 0]); // 2^120 - 1

/// Maximum protocol fee: 5% scaled by 1e6 (50_000).
pub const MAX_PROTOCOL_FEE: u32 = 50_000;

/// Maximum absolute error when decoding a Q96 fixed-point value to f64.
///
/// The conversion `(value * 1e6) / Q96` uses integer division, which
/// truncates the remainder. The truncation loses at most 1 unit in the
/// intermediate integer, mapping to `1 / 1e6 = 0.000001` in the final
/// f64. This bound holds regardless of price magnitude.
pub const Q96_PRECISION: f64 = 0.000001;

/// ERC721 name for PerpCity position NFTs.
pub const ERC721_NAME: &str = "Perp City Positions";

/// ERC721 symbol for PerpCity position NFTs.
pub const ERC721_SYMBOL: &str = "PERPCITY";

#[cfg(test)]
mod tests {
    use super::*;

    // Verify hand-crafted `from_limbs` constants match computed values.

    #[test]
    fn q96_equals_two_pow_96() {
        assert_eq!(Q96, U256::from(1u64) << 96);
    }

    #[test]
    fn q96_u128_equals_two_pow_96() {
        assert_eq!(Q96_U128, 79_228_162_514_264_337_593_543_950_336);
    }

    #[test]
    fn one_half_is_half_of_scale() {
        assert_eq!(ONE_HALF, SCALE_1E6 / 2);
    }

    #[test]
    fn wad_value() {
        assert_eq!(WAD, U256::from(10u64).pow(U256::from(18)));
    }

    #[test]
    fn wad_one_percent_value() {
        assert_eq!(WAD_ONE_PERCENT, U256::from(10u64).pow(U256::from(16)));
    }

    #[test]
    fn tick_range_symmetric() {
        assert_eq!(MIN_TICK, -MAX_TICK);
    }

    #[test]
    fn accounting_token_supply_is_uint120_max() {
        assert_eq!(
            ACCOUNTING_TOKEN_SUPPLY,
            (U256::from(1u64) << 120) - U256::from(1u64)
        );
    }

    #[test]
    fn min_sqrt_price_less_than_max() {
        assert!(MIN_SQRT_PRICE_X96 < MAX_SQRT_PRICE_X96);
    }

    // Verify uint! macro values match the Constants.sol source strings.

    #[test]
    fn min_sqrt_price_x96_matches_contract() {
        let expected = U256::from_str_radix("2505414483750479311864138016", 10).unwrap();
        assert_eq!(MIN_SQRT_PRICE_X96, expected);
    }

    #[test]
    fn max_sqrt_price_x96_matches_contract() {
        let expected = U256::from_str_radix("2505414483750479311864138015696", 10).unwrap();
        assert_eq!(MAX_SQRT_PRICE_X96, expected);
    }
}
