//! The deployed pricing module's fair price — the contract's mark.
//!
//! `PerpLogic.accrue` sets `snap.markPrice = pricing.fairPrice(spots.ammPrice,
//! spots.index, emas.ammPrice, emas.index)`, with `emas` advanced to the
//! block timestamp ([`crate::math::ema::calculate_emas`]). Every health
//! check, `valPnl`, liquidation test and the utilization accrual leg price
//! at this fair price, never at the raw pool price.
//!
//! The live module is perpcity-contracts `test/mocks/Pricing.sol`, which
//! `script/Deploy.s.sol` deploys as the production module (on Arbitrum One
//! at `0xf4689da0cac3f23a04145236dbfe81c3c58cfe22`, per HORMUZ-TRAFFIC
//! `0x137e…5b17`'s `modules()` on 2026-09-07):
//!
//! ```solidity
//! function fairPrice(uint256 ammPrice, uint256 index, uint256 emaAmmPrice, uint256 emaIndex)
//!     external pure returns (uint256)
//! {
//!     uint256 adjustedIndex = index + emaAmmPrice;
//!     uint256 delta = adjustedIndex > emaIndex ? adjustedIndex - emaIndex : 0;
//!     return FixedPointMathLib.avg(ammPrice, delta);
//! }
//! ```

use alloy::primitives::U256;

/// Solady `FixedPointMathLib.avg`: `floor((a + b) / 2)` without the
/// intermediate sum, so it cannot overflow.
fn avg(a: U256, b: U256) -> U256 {
    (a & b) + ((a ^ b) >> 1)
}

/// The deployed `fairPrice`, exact in X96 (see the module docs).
///
/// `index + emaAmmPrice` cannot overflow for chain-sourced inputs (both are
/// `uint128` on chain); the port saturates instead of panicking on
/// out-of-domain callers, where the contract would revert.
pub fn fair_price_x96(
    amm_price_x96: U256,
    index_x96: U256,
    ema_amm_price_x96: U256,
    ema_index_x96: U256,
) -> U256 {
    let adjusted_index = index_x96.saturating_add(ema_amm_price_x96);
    let delta = adjusted_index.saturating_sub(ema_index_x96);
    avg(amm_price_x96, delta)
}

/// [`fair_price_x96`] in human units, for f64 simulators:
/// `(amm + max(index + ema_amm − ema_index, 0)) / 2`.
pub fn fair_price(amm_price: f64, index: f64, ema_amm_price: f64, ema_index: f64) -> f64 {
    let delta = (index + ema_amm_price - ema_index).max(0.0);
    (amm_price + delta) / 2.0
}

#[cfg(test)]
mod tests {
    use alloy::primitives::uint;

    use super::*;
    use crate::constants::Q96_PRECISION;
    use crate::convert::price_x96_to_f64;

    /// Golden vectors verified 2026-09-07 by `eth_call` to the deployed
    /// pricing module `0xf4689da0cac3f23a04145236dbfe81c3c58cfe22` on
    /// Arbitrum One. The first is HORMUZ-TRAFFIC's live `poolState().ammPrice`
    /// and `emas()` at the time, with index = 40 × 2^96.
    #[test]
    fn matches_deployed_fair_price() {
        assert_eq!(
            fair_price_x96(
                uint!(0x2e8d7e0b44090ee63765fb855f_U256),
                uint!(0x280000000000000000000000000_U256),
                uint!(0x2e4e1d5d09c24b2a50779abaf3_U256),
                uint!(0x2dc0f47dc4c7764d34d2f6a88f_U256),
            ),
            uint!(0x1578d53754481f1e1a9854fcbe1_U256)
        );
        // The clamp: index + emaAmm < emaIndex gives delta 0.
        assert_eq!(
            fair_price_x96(
                U256::from(1001u32),
                U256::ONE,
                U256::ONE,
                U256::from(1_000_000u32)
            ),
            U256::from(500u32)
        );
        // Floor of an odd sum.
        assert_eq!(
            fair_price_x96(
                U256::from(7u8),
                U256::from(3u8),
                U256::from(4u8),
                U256::from(2u8)
            ),
            U256::from(6u8)
        );
    }

    #[test]
    fn saturates_instead_of_panicking() {
        assert_eq!(
            fair_price_x96(U256::MAX, U256::MAX, U256::MAX, U256::ZERO),
            U256::MAX
        );
        assert_eq!(
            fair_price_x96(U256::ZERO, U256::ZERO, U256::ZERO, U256::MAX),
            U256::ZERO
        );
    }

    #[test]
    fn f64_helper_agrees_with_x96() {
        let amm = uint!(0x2e8d7e0b44090ee63765fb855f_U256);
        let index = uint!(0x280000000000000000000000000_U256);
        let ema_amm = uint!(0x2e4e1d5d09c24b2a50779abaf3_U256);
        let ema_index = uint!(0x2dc0f47dc4c7764d34d2f6a88f_U256);
        let exact = price_x96_to_f64(fair_price_x96(amm, index, ema_amm, ema_index)).unwrap();
        let approx = fair_price(
            price_x96_to_f64(amm).unwrap(),
            price_x96_to_f64(index).unwrap(),
            price_x96_to_f64(ema_amm).unwrap(),
            price_x96_to_f64(ema_index).unwrap(),
        );
        // `price_x96_to_f64` rounds to Q96_PRECISION, so agree to that.
        assert!(
            (exact - approx).abs() < Q96_PRECISION,
            "{exact} vs {approx}"
        );

        assert_eq!(fair_price(1001.0, 1.0, 1.0, 1_000_000.0), 500.5);
        assert_eq!(fair_price(7.0, 3.0, 4.0, 2.0), 6.0);
    }
}
