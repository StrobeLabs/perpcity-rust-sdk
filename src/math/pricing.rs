//! The market's prices and how the contract marks from them.
//!
//! Three prices, one relation. The pool price (`ammPrice` on chain) is the
//! AMM's; the index is the beacon's; the contract smooths both into a
//! [`PricePair`] of EMAs ([`calculate_emas`], exact to the Solady
//! arithmetic); and the mark is the fair price of all four. `PerpLogic.accrue`
//! sets `snap.markPrice = pricing.fairPrice(spots.ammPrice, spots.index,
//! emas.ammPrice, emas.index)` with `emas` advanced to the block timestamp —
//! [`Mark`] is those inputs at one block. Every health check, `valPnl`,
//! liquidation test and the utilization accrual leg price at this fair
//! price, never at the raw pool price.
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

use alloy::primitives::{I256, U256, uint};
use serde::{Deserialize, Serialize};

use crate::errors::ValidationError;
use crate::math::BlockContext;
use crate::math::fixed_point::exp_wad;

const WAD_U256: U256 = uint!(1_000_000_000_000_000_000_U256);

/// An `(amm, index)` price pair, mirroring the contract's `PricePair` struct
/// (both prices scaled by 2^96).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PricePair {
    /// AMM (pool) price observation or EMA.
    pub amm: u128,
    /// Beacon index observation or EMA.
    pub index: u128,
}

impl PricePair {
    /// Narrow a pair of X96 `uint256` observations (`poolState().ammPrice`,
    /// the beacon's `index()`) to the contract's `uint128` pair.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] when either exceeds `u128::MAX` — the
    /// contract would revert on the same cast.
    pub fn try_from_x96(amm: U256, index: U256) -> Result<Self, ValidationError> {
        let narrow = |value: U256, what: &str| {
            u128::try_from(value).map_err(|_| ValidationError::Overflow {
                context: format!("{what} exceeds uint128"),
            })
        };
        Ok(Self {
            amm: narrow(amm, "AMM price")?,
            index: narrow(index, "index")?,
        })
    }
}

/// Advance a stored EMA pair to `timestamp` using the supplied spot
/// observations and the contract EMA window.
pub fn calculate_emas(
    stored: PricePair,
    spot: PricePair,
    last_touch: u64,
    timestamp: u64,
    ema_window: u64,
) -> Result<PricePair, ValidationError> {
    if timestamp <= last_touch {
        return Ok(stored);
    }
    if ema_window == 0 {
        return Err(ValidationError::InvalidConfig {
            reason: "EMA window is zero".into(),
        });
    }
    let dt = timestamp - last_touch;
    let ratio_wad =
        U256::from(dt)
            .checked_mul(WAD_U256)
            .ok_or_else(|| ValidationError::Overflow {
                context: "EMA dt".into(),
            })?
            / U256::from(ema_window);
    let alpha = exp_wad(
        -I256::try_from(ratio_wad).map_err(|_| ValidationError::Overflow {
            context: "EMA alpha input".into(),
        })?,
    )?;
    let one_minus = WAD_U256 - alpha;
    let amm = (U256::from(stored.amm) * alpha + U256::from(spot.amm) * one_minus) / WAD_U256;
    let index = (U256::from(stored.index) * alpha + U256::from(spot.index) * one_minus) / WAD_U256;
    Ok(PricePair {
        amm: amm.to::<u128>(),
        index: index.to::<u128>(),
    })
}

/// The contract's mark at one block, read by
/// [`MarketReader::get_fair_price`](crate::MarketReader::get_fair_price).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FairPrice {
    /// The block the price inputs were read at.
    pub block: BlockContext,
    /// [`fair_price_x96`] of the block's pool price, beacon index and EMAs.
    pub price_x96: U256,
}

/// What the contract marks from at one block: the pool price, the beacon
/// index, and the stored EMAs advanced to the block timestamp, as
/// `PerpLogic.accrue` sees them. Read by
/// [`StateAt::mark`](crate::StateAt::mark).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    /// The block the inputs were read at.
    pub block: BlockContext,
    /// `poolState().ammPrice`.
    pub amm_price_x96: U256,
    /// The beacon's `index()`.
    pub index_x96: U256,
    /// The EMAs as of the block timestamp.
    pub emas: PricePair,
}

impl Mark {
    /// The mark from the raw views: the stored EMAs, last touched at
    /// `last_touch`, advanced to the block timestamp.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] when a price exceeds `uint128`, and
    /// [`ValidationError::InvalidConfig`] on a zero EMA window with time to
    /// advance across.
    pub fn advanced(
        block: BlockContext,
        amm_price_x96: U256,
        index_x96: U256,
        stored_emas: PricePair,
        last_touch: u64,
        ema_window: u64,
    ) -> Result<Self, ValidationError> {
        let spot = PricePair::try_from_x96(amm_price_x96, index_x96)?;
        let emas = calculate_emas(stored_emas, spot, last_touch, block.timestamp, ema_window)?;
        Ok(Self {
            block,
            amm_price_x96,
            index_x96,
            emas,
        })
    }

    /// [`fair_price_x96`] of these inputs: the price the contract marks at.
    pub fn fair_price_x96(&self) -> U256 {
        fair_price_x96(
            self.amm_price_x96,
            self.index_x96,
            U256::from(self.emas.amm),
            U256::from(self.emas.index),
        )
    }
}

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
    use crate::constants::{Q96, Q96_PRECISION};
    use crate::convert::price_x96_to_f64;

    #[test]
    fn ema_stays_put_at_same_timestamp() {
        let stored = PricePair {
            amm: 100,
            index: 200,
        };
        let spot = PricePair {
            amm: 300,
            index: 400,
        };
        assert_eq!(calculate_emas(stored, spot, 10, 10, 3600).unwrap(), stored);
    }

    /// With no time since the last touch the stored EMAs stand, and with
    /// every input equal the fair price is that price.
    #[test]
    fn a_mark_with_nothing_to_advance_prices_at_its_inputs() {
        let block = BlockContext {
            timestamp: 1_700_000_000,
            ..BlockContext::default()
        };
        let one = Q96.to::<u128>();
        let stored = PricePair {
            amm: one,
            index: one,
        };
        let mark = Mark::advanced(block, Q96, Q96, stored, block.timestamp, 3_600).unwrap();
        assert_eq!(mark.emas, stored);
        assert_eq!(mark.fair_price_x96(), Q96);
    }

    /// Time to advance across with no window to advance by is the
    /// contract's misconfiguration, surfaced rather than divided by.
    #[test]
    fn a_zero_window_cannot_advance_a_mark() {
        let block = BlockContext {
            timestamp: 1_700_000_001,
            ..BlockContext::default()
        };
        let one = Q96.to::<u128>();
        let stored = PricePair {
            amm: one,
            index: one,
        };
        assert!(matches!(
            Mark::advanced(block, Q96, Q96, stored, block.timestamp - 1, 0),
            Err(ValidationError::InvalidConfig { .. })
        ));
    }

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
