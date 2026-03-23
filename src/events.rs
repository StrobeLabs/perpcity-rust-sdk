//! Event decoding for PerpManager and Beacon contracts.
//!
//! Decodes raw [`Log`] entries from WebSocket subscriptions into typed
//! [`MarketEvent`] values. Consumers get human-readable f64 prices and
//! amounts without touching ABI encoding or Q96 math.
//!
//! # Usage
//!
//! ```rust,no_run
//! use perpcity_sdk::events::{MarketEvent, decode_log};
//! # use alloy::rpc::types::Log;
//! # fn example(log: &Log) {
//! if let Some(event) = decode_log(log) {
//!     match event {
//!         MarketEvent::PositionOpened { mark_price, pos_id, .. } => {
//!             println!("position {pos_id} opened at {mark_price}");
//!         }
//!         _ => {}
//!     }
//! }
//! # }
//! ```

use alloy::primitives::{B256, U256};
use alloy::rpc::types::Log;
use alloy::sol_types::SolEvent;

use crate::contracts::{IBeacon, PerpManager};
use crate::convert::{price_x96_to_f64, scale_from_6dec, sqrt_price_x96_to_price};

/// A decoded market event with human-readable f64 values.
#[derive(Debug, Clone)]
pub enum MarketEvent {
    /// A position was opened.
    PositionOpened {
        perp_id: B256,
        mark_price: f64,
        long_oi: f64,
        short_oi: f64,
        pos_id: U256,
        is_maker: bool,
        perp_delta: f64,
        usd_delta: f64,
        tick_lower: i32,
        tick_upper: i32,
    },
    /// Notional was adjusted (swap).
    NotionalAdjusted {
        perp_id: B256,
        mark_price: f64,
        long_oi: f64,
        short_oi: f64,
        pos_id: U256,
        new_perp_delta: f64,
        swap_perp_delta: f64,
        swap_usd_delta: f64,
        funding: f64,
        trading_fees: f64,
    },
    /// A position was closed (or liquidated).
    PositionClosed {
        perp_id: B256,
        mark_price: f64,
        long_oi: f64,
        short_oi: f64,
        pos_id: U256,
        was_maker: bool,
        was_liquidated: bool,
        was_partial_close: bool,
        exit_perp_delta: f64,
        exit_usd_delta: f64,
        net_margin: f64,
        funding: f64,
    },
    /// Index price updated (from Beacon contract).
    IndexUpdated { index: f64 },
}

/// Decode a raw Alloy [`Log`] into a [`MarketEvent`], if recognized.
///
/// Returns `None` for unrecognized events (`MarginAdjusted`, `PerpCreated`,
/// module registration events, ERC20 events, etc.).
///
/// # Errors
///
/// Returns `None` (not an error) if ABI decoding or value conversion fails.
/// This is intentional — a malformed log should not crash the event stream.
pub fn decode_log(log: &Log) -> Option<MarketEvent> {
    let topic0 = *log.topic0()?;

    if topic0 == PerpManager::PositionOpened::SIGNATURE_HASH {
        decode_position_opened(log)
    } else if topic0 == PerpManager::NotionalAdjusted::SIGNATURE_HASH {
        decode_notional_adjusted(log)
    } else if topic0 == PerpManager::PositionClosed::SIGNATURE_HASH {
        decode_position_closed(log)
    } else if topic0 == IBeacon::IndexUpdated::SIGNATURE_HASH {
        decode_index_updated(log)
    } else {
        None
    }
}

fn decode_position_opened(log: &Log) -> Option<MarketEvent> {
    let decoded = PerpManager::PositionOpened::decode_raw_log(
        log.inner.data.topics().iter().copied(),
        log.inner.data.data.as_ref(),
    )
    .ok()?;

    Some(MarketEvent::PositionOpened {
        perp_id: decoded.perpId,
        mark_price: sqrt_price_x96_to_price(decoded.sqrtPriceX96).ok()?,
        long_oi: scale_from_6dec(decoded.longOI.try_into().ok()?),
        short_oi: scale_from_6dec(decoded.shortOI.try_into().ok()?),
        pos_id: decoded.posId,
        is_maker: decoded.isMaker,
        perp_delta: scale_from_6dec(decoded.perpDelta.try_into().ok()?),
        usd_delta: scale_from_6dec(decoded.usdDelta.try_into().ok()?),
        tick_lower: decoded.tickLower.as_i32(),
        tick_upper: decoded.tickUpper.as_i32(),
    })
}

fn decode_notional_adjusted(log: &Log) -> Option<MarketEvent> {
    let decoded = PerpManager::NotionalAdjusted::decode_raw_log(
        log.inner.data.topics().iter().copied(),
        log.inner.data.data.as_ref(),
    )
    .ok()?;

    Some(MarketEvent::NotionalAdjusted {
        perp_id: decoded.perpId,
        mark_price: sqrt_price_x96_to_price(decoded.sqrtPriceX96).ok()?,
        long_oi: scale_from_6dec(decoded.longOI.try_into().ok()?),
        short_oi: scale_from_6dec(decoded.shortOI.try_into().ok()?),
        pos_id: decoded.posId,
        new_perp_delta: scale_from_6dec(decoded.newPerpDelta.try_into().ok()?),
        swap_perp_delta: scale_from_6dec(decoded.swapPerpDelta.try_into().ok()?),
        swap_usd_delta: scale_from_6dec(decoded.swapUsdDelta.try_into().ok()?),
        funding: scale_from_6dec(decoded.funding.try_into().ok()?),
        trading_fees: scale_from_6dec(decoded.tradingFees.try_into().ok()?),
    })
}

fn decode_position_closed(log: &Log) -> Option<MarketEvent> {
    let decoded = PerpManager::PositionClosed::decode_raw_log(
        log.inner.data.topics().iter().copied(),
        log.inner.data.data.as_ref(),
    )
    .ok()?;

    Some(MarketEvent::PositionClosed {
        perp_id: decoded.perpId,
        mark_price: sqrt_price_x96_to_price(decoded.sqrtPriceX96).ok()?,
        long_oi: scale_from_6dec(decoded.longOI.try_into().ok()?),
        short_oi: scale_from_6dec(decoded.shortOI.try_into().ok()?),
        pos_id: decoded.posId,
        was_maker: decoded.wasMaker,
        was_liquidated: decoded.wasLiquidated,
        was_partial_close: decoded.wasPartialClose,
        exit_perp_delta: scale_from_6dec(decoded.exitPerpDelta.try_into().ok()?),
        exit_usd_delta: scale_from_6dec(decoded.exitUsdDelta.try_into().ok()?),
        net_margin: scale_from_6dec(decoded.netMargin.try_into().ok()?),
        funding: scale_from_6dec(decoded.funding.try_into().ok()?),
    })
}

fn decode_index_updated(log: &Log) -> Option<MarketEvent> {
    let decoded = IBeacon::IndexUpdated::decode_raw_log(
        log.inner.data.topics().iter().copied(),
        log.inner.data.data.as_ref(),
    )
    .ok()?;

    Some(MarketEvent::IndexUpdated {
        index: price_x96_to_f64(decoded.index).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, LogData, Signed, U256};
    use alloy::rpc::types::Log as RpcLog;

    use crate::constants::{Q96, Q96_PRECISION};

    /// Build a synthetic RPC Log from an event that implements SolEvent.
    fn make_log<E: SolEvent>(event: &E, address: Address) -> RpcLog {
        let log_data = event.encode_log_data();
        RpcLog {
            inner: alloy::primitives::Log {
                address,
                data: log_data,
            },
            block_hash: None,
            block_number: None,
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        }
    }

    #[test]
    fn decode_position_opened_event() {
        let perp_id = B256::repeat_byte(0x01);
        let event = PerpManager::PositionOpened {
            perpId: perp_id,
            sqrtPriceX96: Q96, // price = 1.0
            longOI: U256::from(1_000_000u64),
            shortOI: U256::from(500_000u64),
            posId: U256::from(42u64),
            isMaker: false,
            perpDelta: alloy::primitives::I256::try_from(100_000_000i64).unwrap(),
            usdDelta: alloy::primitives::I256::try_from(-100_000_000i64).unwrap(),
            tickLower: Signed::try_from(-100i32).unwrap(),
            tickUpper: Signed::try_from(100i32).unwrap(),
        };

        let log = make_log(&event, Address::ZERO);
        let decoded = decode_log(&log).expect("should decode PositionOpened");

        match decoded {
            MarketEvent::PositionOpened {
                perp_id: pid,
                mark_price,
                pos_id,
                is_maker,
                ..
            } => {
                assert_eq!(pid, perp_id);
                assert!((mark_price - 1.0).abs() < Q96_PRECISION);
                assert_eq!(pos_id, U256::from(42u64));
                assert!(!is_maker);
            }
            _ => panic!("expected PositionOpened"),
        }
    }

    #[test]
    fn decode_index_updated_event() {
        let event = IBeacon::IndexUpdated {
            index: Q96 * U256::from(100u64), // index = 100.0
        };

        let log = make_log(&event, Address::ZERO);
        let decoded = decode_log(&log).expect("should decode IndexUpdated");

        match decoded {
            MarketEvent::IndexUpdated { index } => {
                assert!((index - 100.0).abs() < Q96_PRECISION);
            }
            _ => panic!("expected IndexUpdated"),
        }
    }

    #[test]
    fn decode_position_closed_event() {
        let perp_id = B256::repeat_byte(0x02);
        let event = PerpManager::PositionClosed {
            perpId: perp_id,
            sqrtPriceX96: Q96 * U256::from(10u64), // sqrt price → price = 100.0
            longOI: U256::from(2_000_000u64),
            shortOI: U256::from(1_000_000u64),
            posId: U256::from(7u64),
            wasMaker: false,
            wasLiquidated: true,
            wasPartialClose: false,
            exitPerpDelta: alloy::primitives::I256::try_from(-50_000_000i64).unwrap(),
            exitUsdDelta: alloy::primitives::I256::try_from(50_000_000i64).unwrap(),
            tickLower: Signed::try_from(0i32).unwrap(),
            tickUpper: Signed::try_from(0i32).unwrap(),
            netUsdDelta: alloy::primitives::I256::try_from(48_000_000i64).unwrap(),
            funding: alloy::primitives::I256::try_from(-1_000_000i64).unwrap(),
            utilizationFee: U256::from(500_000u64),
            adl: U256::ZERO,
            liquidationFee: U256::from(1_000_000u64),
            netMargin: alloy::primitives::I256::try_from(45_000_000i64).unwrap(),
        };

        let log = make_log(&event, Address::ZERO);
        let decoded = decode_log(&log).expect("should decode PositionClosed");

        match decoded {
            MarketEvent::PositionClosed {
                perp_id: pid,
                mark_price,
                pos_id,
                was_liquidated,
                net_margin,
                funding,
                ..
            } => {
                assert_eq!(pid, perp_id);
                assert!((mark_price - 100.0).abs() < Q96_PRECISION);
                assert_eq!(pos_id, U256::from(7u64));
                assert!(was_liquidated);
                assert!((net_margin - 45.0).abs() < Q96_PRECISION);
                assert!((funding - (-1.0)).abs() < Q96_PRECISION);
            }
            _ => panic!("expected PositionClosed"),
        }
    }

    #[test]
    fn unrecognized_event_returns_none() {
        // A log with an unknown topic0 should return None.
        let log = RpcLog {
            inner: alloy::primitives::Log {
                address: Address::ZERO,
                data: LogData::new_unchecked(vec![B256::repeat_byte(0xFF)], vec![].into()),
            },
            block_hash: None,
            block_number: None,
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        };
        assert!(decode_log(&log).is_none());
    }

    #[test]
    fn empty_log_returns_none() {
        let log = RpcLog {
            inner: alloy::primitives::Log {
                address: Address::ZERO,
                data: LogData::new_unchecked(vec![], vec![].into()),
            },
            block_hash: None,
            block_number: None,
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        };
        assert!(decode_log(&log).is_none());
    }
}
