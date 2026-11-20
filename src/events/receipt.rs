//! V1 trade receipt decoding. The expected event type selects the ABI.
use alloy::primitives::{Address, B256, U256};
use alloy::rpc::types::TransactionReceipt;
use alloy::sol_types::SolEvent;

use crate::contracts::v1::Perp;
use crate::errors::ContractError;
use crate::units::UsdcAtoms;

use super::decode_raw;

/// Position information emitted by a v1 trade or ownership change.
pub trait V1PositionEvent: SolEvent {
    /// Position named by the event.
    fn position_id(&self) -> U256;
    /// Post-trade margin when this event reports it.
    fn post_trade_margin(&self) -> Option<UsdcAtoms>;
}

macro_rules! position_event {
    ($event:ident, $value:ident => $margin:expr) => {
        impl V1PositionEvent for Perp::$event {
            fn position_id(&self) -> U256 {
                self.posId
            }

            fn post_trade_margin(&self) -> Option<UsdcAtoms> {
                let $value = self;
                $margin
            }
        }
    };
}

position_event!(TakerOpened, event => Some(UsdcAtoms::new(event.margin)));
position_event!(TakerAdjusted, event => Some(UsdcAtoms::new(event.margin)));
position_event!(TakerClosed, event => Some(UsdcAtoms::new(event.margin)));
position_event!(MakerOpened, _event => None);
position_event!(MakerAdjusted, event => Some(UsdcAtoms::new(event.margin)));
position_event!(MakerClosed, event => Some(UsdcAtoms::new(event.margin)));
position_event!(PositionOwnerUpdated, _event => None);

/// A mined transaction and its expected v1 event. Taker events carry
/// the complete eight-field swap result, including the fee fund share.
#[derive(Debug)]
pub struct V1TradeResult<E> {
    /// Mined transaction hash.
    pub tx_hash: B256,
    /// Position named by the market.
    pub pos_id: U256,
    /// Post-trade margin in USDC atoms, when emitted.
    pub margin: Option<UsdcAtoms>,
    /// Full event as emitted, including indexed fields.
    pub event: E,
}

/// Find the expected event from this market, optionally for a known position.
/// On a missing or unreadable event the receipt is final and the trade must
/// not be sent again.
pub fn parse_v1_receipt<E: V1PositionEvent>(
    receipt: &TransactionReceipt,
    perp: Address,
    pos_id: Option<U256>,
) -> Result<V1TradeResult<E>, ContractError> {
    for log in receipt.inner.logs() {
        if log.address() == perp
            && log.topics().first() == Some(&E::SIGNATURE_HASH)
            && let Ok(event) = decode_raw::<E>(log)
            && pos_id.is_none_or(|id| event.position_id() == id)
        {
            return Ok(V1TradeResult {
                tx_hash: receipt.transaction_hash,
                pos_id: event.position_id(),
                margin: event.post_trade_margin(),
                event,
            });
        }
    }
    Err(ContractError::MinedEventMissing {
        tx_hash: receipt.transaction_hash,
        event: E::SIGNATURE,
    })
}
