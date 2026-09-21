//! Transaction lifecycle errors.

use alloy::primitives::FixedBytes;
use alloy::transports::TransportError;
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
        /// Hash of the mined transaction.
        tx_hash: FixedBytes<32>,
        /// Human-readable description.
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
    ///
    /// The transaction was broadcast and may still mine, so its nonce stays
    /// consumed. Look up `tx_hash` later (for example with
    /// [`PerpClient::poll_receipt`](crate::PerpClient::poll_receipt)) to
    /// learn the outcome.
    #[error("receipt timeout for {tx_hash}: {reason}")]
    ReceiptTimeout {
        /// Hash of the broadcast transaction.
        tx_hash: FixedBytes<32>,
        /// Why polling stopped: no receipt by the deadline, or the last
        /// poll's RPC error.
        reason: String,
    },

    /// The broadcast request failed after the transaction was signed.
    ///
    /// The node may still have accepted it: the request can fail after the
    /// transaction reached the mempool, or after it mined. The send path
    /// treats its nonce as consumed and resyncs from chain before the next
    /// send, so look up `tx_hash` (for example with
    /// [`PerpClient::poll_receipt`](crate::PerpClient::poll_receipt)) to
    /// learn whether it landed.
    ///
    /// Transient, like the transport error it wraps.
    #[error("broadcast failed for {tx_hash}: {source}")]
    BroadcastFailed {
        /// Hash of the signed transaction.
        tx_hash: FixedBytes<32>,
        /// The transport error the broadcast returned.
        #[source]
        source: TransportError,
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
    /// Hash of the signed transaction, for every failure from the broadcast
    /// onward: `BroadcastFailed`, `ReceiptTimeout`, `Reverted` and
    /// `OutOfGas`. `None` means nothing was sent.
    ///
    /// A `Some` hash may have landed on chain even when the error says the
    /// send failed, so look up its receipt before treating the effect as
    /// absent.
    pub fn tx_hash(&self) -> Option<FixedBytes<32>> {
        match self {
            Self::BroadcastFailed { tx_hash, .. }
            | Self::ReceiptTimeout { tx_hash, .. }
            | Self::Reverted { tx_hash, .. }
            | Self::OutOfGas { tx_hash, .. } => Some(*tx_hash),
            _ => None,
        }
    }

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
