//! Transaction lifecycle errors.

use alloy::primitives::FixedBytes;
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
        /// (e.g. `"InvalidMarginRatio"`). Unknown selectors decode to
        /// `"UnknownContractError(0x…)"` with the selector preserved.
        error_name: String,
        /// The raw 4-byte selector (displays as `0x`-prefixed hex, e.g.
        /// `0xbcffc83f`); match it typed via [`Self::is_revert`].
        selector: FixedBytes<4>,
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

    /// Transaction was broadcast and mined but ran out of gas: execution
    /// consumed the limit and returned no revert data. Gas was burned.
    ///
    /// Distinct from [`Self::Reverted`] because the call was never disproved
    /// — only the limit it carried was too small. Arbitrum charges execution
    /// costs that `eth_estimateGas` does not model, so an estimate can be
    /// below what the same call consumes in a block, and `eth_call` does not
    /// reproduce the shortfall either: pre-flight passes and the limit is
    /// only disproved here.
    ///
    /// Not transient, so a backoff loop does not spin on it. The send path
    /// evicts the cached estimate before returning, so a caller-level retry
    /// re-estimates rather than repeating the same limit.
    #[error("transaction out of gas: {tx_hash} used {gas_used} of {gas_limit}")]
    OutOfGas {
        /// Hash of the mined transaction.
        tx_hash: FixedBytes<32>,
        /// Gas the transaction consumed.
        gas_used: u64,
        /// Limit it was broadcast with.
        gas_limit: u64,
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

    /// Pre-flight simulation (`eth_estimateGas` or `eth_call`) failed with
    /// the node's definitive execution answer but no decodable contract
    /// revert: an empty revert (a selector the deployed contract does not
    /// have, a bare `revert()`), or execution running out of gas inside
    /// the pinned limit. The transaction was **not** broadcast.
    ///
    /// Deterministic for the same calldata and chain state, so — unlike
    /// [`Self::GasUnavailable`] — **not** transient: retrying reproduces
    /// it. (`PerpCityError::is_transient` says `false`.)
    #[error("simulation failed: {reason}")]
    SimulationFailed {
        /// The node's error response.
        reason: String,
    },

    /// Gas price or base fee is not available (cache stale, RPC down), or a
    /// pre-flight simulation could not reach the node (transport failure).
    /// Transient: the transaction was neither disproved nor broadcast.
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

impl TransactionError {
    /// Whether this error is a [`Self::SimulationReverted`] carrying the
    /// typed contract error `E`, compared by 4-byte selector — no string
    /// matching.
    ///
    /// ```rust,ignore
    /// use perpcity_sdk::Perp;
    ///
    /// if err.is_revert::<Perp::NotLiquidatable>() {
    ///     // healthy right now — retry later
    /// } else if err.is_revert::<Perp::NonMakerPosition>() {
    ///     // never liquidatable on this path — drop the id
    /// }
    /// ```
    pub fn is_revert<E: alloy::sol_types::SolError>(&self) -> bool {
        matches!(
            self,
            Self::SimulationReverted { selector, .. } if selector.0 == E::SELECTOR
        )
    }
}
