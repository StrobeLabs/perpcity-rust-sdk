//! Transaction lifecycle errors.

use thiserror::Error;

/// Errors arising from the transaction lifecycle: simulation, signing,
/// broadcasting, receipt polling, and gas resolution.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum TransactionError {
    /// Pre-flight simulation (`eth_estimateGas` or `eth_call`) detected a
    /// contract revert. The transaction was **not** broadcast — no gas was
    /// burned.
    #[error("simulation reverted: {error_name} ({selector})")]
    SimulationReverted {
        /// Human-readable error name decoded from the 4-byte selector
        /// (e.g. `"InvalidMarginRatio"`).
        error_name: String,
        /// Raw 4-byte selector as hex (e.g. `"0xbcffc83f"`).
        selector: String,
        /// Full revert data hex, if available.
        revert_data: Option<String>,
    },

    /// Transaction was broadcast and mined but reverted on-chain.
    /// Gas was burned.
    #[error("transaction reverted: {reason}")]
    Reverted {
        /// Human-readable description (typically includes the tx hash).
        reason: String,
    },

    /// Receipt polling timed out before the transaction was confirmed.
    #[error("receipt timeout: {reason}")]
    ReceiptTimeout {
        /// Description including the tx hash.
        reason: String,
    },

    /// Transaction signing failed.
    #[error("signing failed: {reason}")]
    SigningFailed {
        /// The underlying signing error.
        reason: String,
    },

    /// Gas price or base fee is not available (cache stale, RPC down).
    #[error("gas unavailable: {reason}")]
    GasUnavailable {
        /// Description of why gas data is unavailable.
        reason: String,
    },

    /// Too many unconfirmed transactions in the pipeline.
    #[error("too many in-flight: {count} (max {max})")]
    TooManyInFlight {
        /// Current number of in-flight transactions.
        count: usize,
        /// Maximum allowed.
        max: usize,
    },

    /// The local nonce sequence no longer provably matches the chain — a
    /// broadcast failed or a receipt timed out, leaving a nonce's fate
    /// unknowable. Sends fail fast until in-flight work drains, then the
    /// next send resyncs from the chain automatically.
    ///
    /// Transient by design (see `PerpCityError::is_transient`): the
    /// condition clears itself, so callers should back off briefly and
    /// retry rather than treat this as a dead client.
    #[error("nonce desynced from chain ({in_flight} in flight); resyncs when drained")]
    NonceDesynced {
        /// Transactions still awaiting receipts, which block the resync.
        in_flight: usize,
    },
}
