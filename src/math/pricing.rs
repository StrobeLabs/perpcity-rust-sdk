//! The market's prices and how the contract marks from them.
//!
//! Three prices, one relation. The pool price (`ammPrice` on chain) is the
//! AMM's; the index is the beacon's; the contract smooths both into a
//! [`PricePair`] of EMAs ([`calculate_emas`], exact to the Solady
//! arithmetic, the same `_calcEmas` on every build); and the mark is the
//! fair price of all four. `PerpLogic.accrue` sets `snap.markPrice =
//! pricing.fairPrice(spots.ammPrice, spots.index, emas.ammPrice,
//! emas.index)` with `emas` advanced to the block timestamp — [`Mark`] is
//! those inputs at one block, with the pricing module's answer. Every
//! health check, `valPnl`, liquidation test and the utilization accrual
//! leg price at this fair price, never at the raw pool price.
//!
//! The pricing module is the market's, and governance can swap it, so its
//! rule is a fact of the build. A `v1.0.0` market's `Pricing` anchors the
//! mark to the index, `index ± min(|emaAmm − emaIndex|, index ·
//! maxPremiumRatio)`, and ignores the pool price; the SDK asks the module
//! by `eth_call` rather than port it, so [`Mark::price`] is the module's
//! word. The earlier builds' rule is ported below as [`fair_price`].
//!
//! A `v1.0.0` market also refuses its beacon's index when it is older than
//! the market's latency or outside the price domain; [`BeaconIndex`] holds
//! what that test reads.
//!
//! The earlier builds' module is perpcity-contracts `test/mocks/Pricing.sol`, which
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

use crate::constants::{V1_MAX_INDEX_X96, V1_MIN_INDEX_X96};
use crate::errors::ValidationError;
use crate::math::BlockContext;
use crate::units::Price;
use crate::units::fixed_point::exp_wad;

const WAD_U256: U256 = uint!(1_000_000_000_000_000_000_U256);

/// An `(amm, index)` price pair, mirroring the contract's `PricePair` struct
/// (both prices scaled by 2^96).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// What the contract marks from at one block: the pool price, the beacon
/// index, and the stored EMAs advanced to the block timestamp, as
/// `PerpLogic.accrue` sees them, and the mark its pricing module makes of
/// them. Read by [`StateAt::mark`](crate::StateAt::mark) and, at the
/// lagged snapshot block,
/// [`MarketReader::get_mark`](crate::MarketReader::get_mark).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    /// The block the inputs were read at.
    pub block: BlockContext,
    /// `poolState().ammPrice`.
    pub pool_price: Price,
    /// The beacon's `index()`.
    pub index: Price,
    /// The EMAs as of the block timestamp.
    pub emas: PricePair,
    /// The pricing module's `fairPrice` of the four: on the earlier builds
    /// the port [`fair_price`], on `v1.0.0` the module's own answer by
    /// `eth_call` at the block.
    pub price: Price,
}

impl Mark {
    /// The mark from the raw views on the two earlier builds: the stored
    /// EMAs, last touched at `last_touch`, advanced to the block timestamp,
    /// and priced by their module's rule, [`fair_price`]. A `v1.0.0`
    /// market's module is asked instead ([`StateAt::mark`](crate::StateAt::mark)).
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] when a price exceeds `uint128`, and
    /// [`ValidationError::InvalidConfig`] on a zero EMA window with time to
    /// advance across.
    pub fn advanced(
        block: BlockContext,
        pool_price: Price,
        index: Price,
        stored_emas: PricePair,
        last_touch: u64,
        ema_window: u64,
    ) -> Result<Self, ValidationError> {
        let spot = PricePair::try_from_x96(pool_price.x96(), index.x96())?;
        let emas = calculate_emas(stored_emas, spot, last_touch, block.timestamp, ema_window)?;
        let price = fair_price(
            pool_price,
            index,
            Price::from_x96(U256::from(emas.amm)),
            Price::from_x96(U256::from(emas.index)),
        );
        Ok(Self {
            block,
            pool_price,
            index,
            emas,
            price,
        })
    }

    /// The price the contract marks at: [`Self::price`].
    pub fn fair_price(&self) -> Price {
        self.price
    }
}

/// The index a `v1.0.0` market takes from its beacon at one block, with
/// what decides whether it takes it: the beacon's `index()` returns the
/// value and its update time, and the market refuses the value
/// (`SafeBeaconIndex.read`) when it is older than the market's
/// `latency()` (`StaleBeaconIndex`), dated after the block, or outside
/// `[V1_MIN_INDEX_X96, V1_MAX_INDEX_X96]` (`PriceOutsideRange`). Every
/// action reverts then, liquidations included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeaconIndex {
    /// The block the index, its time and the latency were read at.
    pub block: BlockContext,
    /// The beacon's value.
    pub index: Price,
    /// Unix timestamp the beacon last took a value.
    pub updated_at: u64,
    /// The market's `latency()`: the oldest index it accepts, in seconds.
    /// Governance can change it (`setLatency`).
    pub latency: u32,
}

impl BeaconIndex {
    /// Whether the market refuses this index at `timestamp` with
    /// `StaleBeaconIndex`: the update is after `timestamp`, or more than
    /// [`Self::latency`] seconds before it. A send lands in a later block
    /// than the read, so the caller asks at the time it expects to land.
    pub fn is_stale_at(&self, timestamp: u64) -> bool {
        self.updated_at > timestamp || timestamp - self.updated_at > u64::from(self.latency)
    }

    /// The last timestamp at which the market still accepts this index:
    /// the update time plus the latency.
    pub fn accepted_until(&self) -> u64 {
        self.updated_at.saturating_add(u64::from(self.latency))
    }

    /// Whether the index is inside the domain the market accepts, where a
    /// value outside it reverts `PriceOutsideRange` however fresh it is.
    pub fn is_in_domain(&self) -> bool {
        (V1_MIN_INDEX_X96..=V1_MAX_INDEX_X96).contains(&self.index.x96())
    }
}

/// The stored EMA pair with the touch it was stored at: what `emas()` and
/// `rates().lastTouch` hold, and what the `RatesAndEmasRefreshed` event
/// carries. A [`PricePair`] that knows when it is current, for a live
/// cache that follows the feed and must mark between touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Emas {
    /// The pool-price EMA.
    pub amm_price: Price,
    /// The index EMA.
    pub index: Price,
    /// Unix timestamp the pair is current as of: the market's last touch.
    pub last_touch: u64,
}

impl Emas {
    /// The pair at `last_touch`, from the contract's stored words.
    pub fn stored(pair: PricePair, last_touch: u64) -> Self {
        Self {
            amm_price: Price::from_x96(U256::from(pair.amm)),
            index: Price::from_x96(U256::from(pair.index)),
            last_touch,
        }
    }

    /// The pair as the contract stores it, `uint128` each.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] when a price exceeds `uint128`.
    pub fn pair(&self) -> Result<PricePair, ValidationError> {
        PricePair::try_from_x96(self.amm_price.x96(), self.index.x96())
    }

    /// The pair advanced to `timestamp` against the spot prices, exactly
    /// as the contract would advance it on a touch then
    /// ([`calculate_emas`]), and current as of `timestamp`. Unchanged when
    /// `timestamp` is not after `last_touch`.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] when a price exceeds `uint128`, and
    /// [`ValidationError::InvalidConfig`] on a zero EMA window with time to
    /// advance across.
    pub fn advanced(
        self,
        pool_price: Price,
        index: Price,
        timestamp: u64,
        ema_window: u64,
    ) -> Result<Self, ValidationError> {
        let spot = PricePair::try_from_x96(pool_price.x96(), index.x96())?;
        let emas = calculate_emas(self.pair()?, spot, self.last_touch, timestamp, ema_window)?;
        Ok(Self::stored(emas, timestamp.max(self.last_touch)))
    }

    /// The earlier builds' mark at `timestamp`: [`fair_price`] of the spot
    /// prices and this pair advanced to it. A `v1.0.0` market's module
    /// prices by another rule, which the SDK asks the module for rather
    /// than port, so a cache of one marks by an `eth_call`
    /// ([`StateAt::mark`](crate::StateAt::mark)), not by this.
    ///
    /// # Errors
    ///
    /// As [`Self::advanced`].
    pub fn mark(
        self,
        pool_price: Price,
        index: Price,
        timestamp: u64,
        ema_window: u64,
    ) -> Result<Price, ValidationError> {
        let emas = self.advanced(pool_price, index, timestamp, ema_window)?;
        Ok(fair_price(pool_price, index, emas.amm_price, emas.index))
    }
}

/// Solady `FixedPointMathLib.avg`: `floor((a + b) / 2)` without the
/// intermediate sum, so it cannot overflow.
fn avg(a: U256, b: U256) -> U256 {
    (a & b) + ((a ^ b) >> 1)
}

/// The deployed `fairPrice`, exact (see the module docs).
///
/// `index + emaAmmPrice` cannot overflow for chain-sourced inputs (both are
/// `uint128` on chain); the port saturates instead of panicking on
/// out-of-domain callers, where the contract would revert.
pub fn fair_price(
    pool_price: Price,
    index: Price,
    ema_pool_price: Price,
    ema_index: Price,
) -> Price {
    let adjusted_index = index.x96().saturating_add(ema_pool_price.x96());
    let delta = adjusted_index.saturating_sub(ema_index.x96());
    Price::from_x96(avg(pool_price.x96(), delta))
}

/// [`fair_price`] in human units, for f64 simulators:
/// `(pool + max(index + ema_pool − ema_index, 0)) / 2`.
///
/// The `_f64` names the lossy twin, since the exact one is the default.
pub fn fair_price_f64(pool_price: f64, index: f64, ema_pool_price: f64, ema_index: f64) -> f64 {
    let delta = (index + ema_pool_price - ema_index).max(0.0);
    (pool_price + delta) / 2.0
}

#[cfg(test)]
mod tests {
    use alloy::primitives::uint;

    use super::*;
    use crate::constants::{Q96, Q96_PRECISION};

    /// A price from its Q96 word.
    fn p(x96: U256) -> Price {
        Price::from_x96(x96)
    }

    /// The same, as the `f64` a person reads.
    fn f(x96: U256) -> f64 {
        p(x96).to_f64().unwrap()
    }

    /// A live accrue, reproduced to the digit: HORMUZ-TRAFFIC's
    /// `RatesAndEmasRefreshed` at Arbitrum One block 510213600 (tx
    /// `0x0961…26c4`, log 8), 300 s after the last touch. The stored pair
    /// and `lastTouch` are the state at block 510213599; the spot pool
    /// price is `poolState().ammPrice` at block 510213600, since the swap
    /// in that transaction ran before the accrue; the index is the beacon's
    /// at either block. The fair price is the deployed pricing module's
    /// `fairPrice` of the result, by `eth_call` at the same block.
    #[test]
    fn advancing_the_mark_reproduces_a_live_accrue() {
        let block = BlockContext {
            number: 510213600,
            timestamp: 1790734624,
            ..BlockContext::default()
        };
        let stored = PricePair {
            amm: 3320491901781519017281026778664,
            index: 3084589206424849219740478559607,
        };
        let mark = Mark::advanced(
            block,
            p(uint!(3333452930552967749837079299470_U256)),
            p(uint!(3248354663084837841335301963776_U256)),
            stored,
            1790734324,
            3600,
        )
        .unwrap();
        assert_eq!(
            mark.emas,
            PricePair {
                amm: 3321528208423946384037633669774,
                index: 3097683169375594803637957648468,
            }
        );
        assert_eq!(
            mark.fair_price(),
            p(uint!(3402826316343078585786028642276_U256))
        );
    }

    /// The stored pair advances to the live accrue's result exactly, and
    /// its mark is the fair price of that result.
    #[test]
    fn the_stored_pair_advances_as_the_contract_did() {
        let (amm, index) = (
            p(uint!(3333452930552967749837079299470_U256)),
            p(uint!(3248354663084837841335301963776_U256)),
        );
        let stored = Emas {
            amm_price: p(uint!(3320491901781519017281026778664_U256)),
            index: p(uint!(3084589206424849219740478559607_U256)),
            last_touch: 1790734324,
        };

        let emas = stored.advanced(amm, index, 1790734624, 3600).unwrap();
        assert_eq!(
            emas,
            Emas {
                amm_price: p(uint!(3321528208423946384037633669774_U256)),
                index: p(uint!(3097683169375594803637957648468_U256)),
                last_touch: 1790734624,
            }
        );
        assert_eq!(
            stored.mark(amm, index, 1790734624, 3600).unwrap(),
            p(uint!(3402826316343078585786028642276_U256))
        );
    }

    /// With no time since the last touch the stored pair stands, and the
    /// mark is the fair price of the spots and that pair.
    #[test]
    fn a_pair_with_nothing_to_advance_stands() {
        let one = Price::try_from(1.0).unwrap();
        let (spot, index) = (
            Price::try_from(1.5).unwrap(),
            Price::try_from(1.25).unwrap(),
        );
        let stored = Emas {
            amm_price: one,
            index: one,
            last_touch: 10,
        };
        assert_eq!(stored.advanced(spot, index, 10, 3_600).unwrap(), stored);
        assert_eq!(
            stored.mark(spot, index, 10, 3_600).unwrap(),
            fair_price(spot, index, one, one)
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
        let mark = Mark::advanced(block, p(Q96), p(Q96), stored, block.timestamp, 3_600).unwrap();
        assert_eq!(mark.emas, stored);
        assert_eq!(mark.fair_price(), p(Q96));
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
            Mark::advanced(block, p(Q96), p(Q96), stored, block.timestamp - 1, 0),
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
            fair_price(
                p(uint!(0x2e8d7e0b44090ee63765fb855f_U256)),
                p(uint!(0x280000000000000000000000000_U256)),
                p(uint!(0x2e4e1d5d09c24b2a50779abaf3_U256)),
                p(uint!(0x2dc0f47dc4c7764d34d2f6a88f_U256)),
            ),
            p(uint!(0x1578d53754481f1e1a9854fcbe1_U256))
        );
        // The clamp: index + emaAmm < emaIndex gives delta 0.
        assert_eq!(
            fair_price(
                p(U256::from(1001u32)),
                p(U256::ONE),
                p(U256::ONE),
                p(U256::from(1_000_000u32))
            ),
            p(U256::from(500u32))
        );
        // Floor of an odd sum.
        assert_eq!(
            fair_price(
                p(U256::from(7u8)),
                p(U256::from(3u8)),
                p(U256::from(4u8)),
                p(U256::from(2u8))
            ),
            p(U256::from(6u8))
        );
    }

    /// `SafeBeaconIndex.read`: an update exactly `latency` old is taken,
    /// one second older or dated after the block is refused, and the
    /// domain's ends are inside it.
    #[test]
    fn a_beacon_index_is_stale_past_its_latency_or_from_the_future() {
        let reading = BeaconIndex {
            block: BlockContext {
                timestamp: 1_000_000,
                ..BlockContext::default()
            },
            index: p(Q96),
            updated_at: 1_000_000 - 600,
            latency: 600,
        };
        assert!(!reading.is_stale_at(1_000_000));
        assert!(reading.is_stale_at(1_000_001));
        assert!(!reading.is_stale_at(reading.updated_at));
        assert!(
            reading.is_stale_at(reading.updated_at - 1),
            "dated after the block"
        );
        assert_eq!(reading.accepted_until(), 1_000_000);

        let at = |index_x96: U256| BeaconIndex {
            index: p(index_x96),
            ..reading
        };
        assert!(at(V1_MIN_INDEX_X96).is_in_domain());
        assert!(at(V1_MAX_INDEX_X96).is_in_domain());
        assert!(!at(V1_MIN_INDEX_X96 - U256::from(1u8)).is_in_domain());
        assert!(!at(V1_MAX_INDEX_X96 + U256::from(1u8)).is_in_domain());
    }

    #[test]
    fn saturates_instead_of_panicking() {
        assert_eq!(
            fair_price(p(U256::MAX), p(U256::MAX), p(U256::MAX), p(U256::ZERO)),
            p(U256::MAX)
        );
        assert_eq!(
            fair_price(p(U256::ZERO), p(U256::ZERO), p(U256::ZERO), p(U256::MAX)),
            p(U256::ZERO)
        );
    }

    #[test]
    fn the_f64_twin_agrees_with_the_exact_one() {
        let amm = uint!(0x2e8d7e0b44090ee63765fb855f_U256);
        let index = uint!(0x280000000000000000000000000_U256);
        let ema_amm = uint!(0x2e4e1d5d09c24b2a50779abaf3_U256);
        let ema_index = uint!(0x2dc0f47dc4c7764d34d2f6a88f_U256);
        let exact = fair_price(p(amm), p(index), p(ema_amm), p(ema_index))
            .to_f64()
            .unwrap();
        let approx = fair_price_f64(f(amm), f(index), f(ema_amm), f(ema_index));
        // The f64 view rounds to Q96_PRECISION, so agree to that.
        assert!(
            (exact - approx).abs() < Q96_PRECISION,
            "{exact} vs {approx}"
        );

        assert_eq!(fair_price_f64(1001.0, 1.0, 1.0, 1_000_000.0), 500.5);
        assert_eq!(fair_price_f64(7.0, 3.0, 4.0, 2.0), 6.0);
    }
}
