//! On-chain / protocol state errors.

use alloy::primitives::U256;
use thiserror::Error;

/// Errors from querying on-chain protocol state.
#[derive(Error, Debug)]
pub enum ContractError {
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

    /// A multicall returned unexpected results (wrong count or subcall failure).
    #[error("multicall failed: {reason}")]
    MulticallFailed {
        /// What went wrong.
        reason: String,
    },
}
