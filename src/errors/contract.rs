//! On-chain / protocol state errors.

use alloy::primitives::{B256, U256};
use thiserror::Error;

/// Errors from querying on-chain protocol state.
#[derive(Error, Debug)]
pub enum ContractError {
    /// The perp does not exist on-chain.
    #[error("perp not found: {perp_id}")]
    PerpNotFound {
        /// The perp ID that was not found.
        perp_id: B256,
    },

    /// The position does not exist on-chain.
    #[error("position not found: id={pos_id}")]
    PositionNotFound {
        /// The position ID that was not found.
        pos_id: U256,
    },

    /// A required module is not registered.
    #[error("module not registered: {module}")]
    ModuleNotRegistered {
        /// Name of the missing module.
        module: String,
    },

    /// An expected event was not found in the transaction receipt.
    #[error("event not found: {event_name}")]
    EventNotFound {
        /// Name of the missing event.
        event_name: String,
    },

    /// A quote simulation (e.g. `quoteClosePosition`) returned a revert reason.
    #[error("quote reverted: {reason}")]
    QuoteReverted {
        /// The hex-encoded revert data.
        reason: String,
    },
}
