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

    /// A block header the read needed to pin to was not available from the
    /// RPC endpoint (e.g. a lagging load-balanced replica).
    #[error("block {number} header unavailable from RPC")]
    BlockUnavailable {
        /// The block number whose header was requested.
        number: u64,
    },

    /// A raw storage read (`eth_getProof`, `eth_getStorageAt`, `extsload`)
    /// failed or returned an unexpected shape.
    ///
    /// The transport error, when one exists, is preserved as the source
    /// (behind an `Arc` so a single failed read shared by several positions
    /// keeps its cause on every affected result).
    #[error("storage read failed: {context}")]
    StorageReadFailed {
        /// What was being read.
        context: String,
        /// The underlying transport error, if the failure came from RPC.
        #[source]
        source: Option<std::sync::Arc<alloy::transports::TransportError>>,
    },
}
