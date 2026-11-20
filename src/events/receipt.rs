//! V1 trade receipt decoding. The expected event type selects the ABI.
use alloy::primitives::{Address, B256, I256, U256};
use alloy::rpc::types::TransactionReceipt;
use alloy::sol_types::SolEvent;

use crate::contracts::v1::Perp;
use crate::errors::{ContractError, Result, TransactionError};
use crate::units::UsdcAtoms;

use super::decode_raw;

mod sealed {
    pub trait Sealed {}
}

/// Position information emitted by a v1 trade or ownership change.
pub trait V1PositionEvent: SolEvent + sealed::Sealed {
    /// Position named by the event.
    fn position_id(&self) -> U256;
    /// Post-trade margin when this event reports it.
    fn post_trade_margin(&self) -> std::result::Result<Option<UsdcAtoms>, &'static str>;
}

macro_rules! position_event {
    ($event:ident, $value:ident => $margin:expr) => {
        impl sealed::Sealed for Perp::$event {}

        impl V1PositionEvent for Perp::$event {
            fn position_id(&self) -> U256 {
                self.posId
            }

            fn post_trade_margin(&self) -> std::result::Result<Option<UsdcAtoms>, &'static str> {
                let $value = self;
                Ok($margin)
            }
        }
    };
}

position_event!(TakerOpened, event => {
    let margin = I256::try_from(event.margin)
        .ok()
        .and_then(|deposit| deposit.checked_sub(event.sr.totalFeeAmt))
        .and_then(|net| u128::try_from(net).ok())
        .ok_or("net taker margin out of range")?;
    Some(UsdcAtoms::new(margin))
});
position_event!(TakerAdjusted, event => Some(UsdcAtoms::new(event.margin)));
position_event!(TakerClosed, event => Some(UsdcAtoms::new(event.margin)));
position_event!(MakerOpened, _event => None);
position_event!(MakerAdjusted, event => Some(UsdcAtoms::new(event.margin)));
position_event!(MakerConverted, event => Some(UsdcAtoms::new(event.margin)));
position_event!(MakerClosed, event => Some(UsdcAtoms::new(event.margin)));
position_event!(PositionOwnerUpdated, _event => None);

/// A mined transaction and its expected v1 event. Taker events carry
/// the complete eight-field swap result, including the fee fund share.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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

impl<E> V1TradeResult<E> {
    #[doc(hidden)]
    pub fn new(tx_hash: B256, pos_id: U256, margin: Option<UsdcAtoms>, event: E) -> Self {
        Self { tx_hash, pos_id, margin, event }
    }
}

/// Find the expected event from this market, optionally for a known position.
/// On a missing or unreadable event the receipt is final and the trade must
/// not be sent again.
pub fn parse_v1_receipt<E: V1PositionEvent>(
    receipt: &TransactionReceipt,
    perp: Address,
    pos_id: Option<U256>,
) -> Result<V1TradeResult<E>> {
    parse_v1_receipt_for(receipt, perp, pos_id, None)
}

/// Match an optional position and indexed owner (topic two).
pub fn parse_v1_receipt_for<E: V1PositionEvent>(
    receipt: &TransactionReceipt,
    perp: Address,
    pos_id: Option<U256>,
    owner: Option<Address>,
) -> Result<V1TradeResult<E>> {
    let tx_hash = receipt.transaction_hash;
    if !receipt.status() {
        return Err(TransactionError::Reverted {
            tx_hash,
            reason: "receipt status is zero".into(),
        }
        .into());
    }
    for log in receipt.inner.logs() {
        let topics = log.topics();
        if log.address() != perp || topics.first() != Some(&E::SIGNATURE_HASH) {
            continue;
        }
        let failed = |reason: String| ContractError::MinedEventInvalid { tx_hash, reason };
        let event = decode_raw::<E>(log).map_err(|e| failed(e.to_string()))?;
        if pos_id.is_some_and(|id| event.position_id() != id)
            || owner.is_some_and(|addr| topics.get(2) != Some(&addr.into_word()))
        {
            continue;
        }
        return Ok(V1TradeResult {
            tx_hash,
            pos_id: event.position_id(),
            margin: event.post_trade_margin().map_err(|e| failed(e.into()))?,
            event,
        });
    }
    Err(ContractError::MinedEventMissing {
        tx_hash,
        event: E::SIGNATURE,
    }
    .into())
}

/// Position minted by this market, optionally restricted to a known ID.
pub fn minted_position_id(
    receipt: &TransactionReceipt,
    perp: Address,
    pos_id: Option<U256>,
) -> Result<U256> {
    parse_v1_receipt_for::<Perp::PositionOwnerUpdated>(receipt, perp, pos_id, Some(Address::ZERO))
        .map(|trade| trade.pos_id)
}

/// Outcome of removing all maker liquidity.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MakerExit {
    /// All inventory closed.
    Closed(V1TradeResult<Perp::MakerClosed>),
    /// Residual inventory became a taker position.
    Converted(V1TradeResult<Perp::MakerConverted>),
}

/// Decode either outcome of removing all liquidity from a known maker.
pub fn parse_maker_exit(
    receipt: &TransactionReceipt,
    perp: Address,
    pos_id: U256,
    owner: Option<Address>,
) -> Result<MakerExit> {
    match parse_v1_receipt_for(receipt, perp, Some(pos_id), owner) {
        Ok(trade) => Ok(MakerExit::Closed(trade)),
        Err(crate::PerpCityError::Contract(ContractError::MinedEventMissing { .. })) => {
            match parse_v1_receipt_for(receipt, perp, Some(pos_id), owner) {
                Ok(trade) => Ok(MakerExit::Converted(trade)),
                Err(crate::PerpCityError::Contract(ContractError::MinedEventMissing { tx_hash, .. })) => {
                    Err(ContractError::MinedEventMissing {
                        tx_hash,
                        event: "MakerClosed or MakerConverted",
                    }.into())
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}
