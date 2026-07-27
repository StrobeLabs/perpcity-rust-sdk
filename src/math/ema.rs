//! Exact Perp EMA arithmetic.
//!
//! The decay factor uses Solady's `expWad`, matching the contract's rational
//! approximation and integer rounding rather than a floating-point `exp`.

use alloy::primitives::{I256, U256, uint};

use crate::errors::ValidationError;

const WAD_U256: U256 = uint!(1_000_000_000_000_000_000_U256);

/// Solady's `expWad` power-of-two reassembly scale.
const EXP_SCALE: U256 = uint!(3822833074963236453042738258902158003155416615667_U256);

/// An `(amm, index)` price pair, mirroring the contract's `PricePair` struct
/// (both prices scaled by 2^96).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PricePair {
    /// AMM (pool) price observation or EMA.
    pub amm: u128,
    /// Beacon index observation or EMA.
    pub index: u128,
}

fn i(value: i128) -> I256 {
    I256::try_from(value).expect("i128 fits I256")
}

/// Solady-compatible `expWad` for the EMA decay domain.
pub fn exp_wad(mut x: I256) -> Result<U256, ValidationError> {
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
}
