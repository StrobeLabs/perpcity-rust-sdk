//! Error types for the PerpCity SDK.
//!
//! Errors are organized by module boundary:
//!
//! - [`TransactionError`] — transaction lifecycle (simulation, signing,
//!   broadcasting, receipt polling, gas resolution)
//! - [`ValidationError`] — input validation (prices, margins, ticks,
//!   leverage, arithmetic overflow)
//! - [`ContractError`] — on-chain protocol state (perps, positions,
//!   modules, events, quotes)
//!
//! The top-level [`PerpCityError`] composes all three via `#[from]`
//! conversions, so module-internal code can return specific error types
//! with `?` and callers receive a unified enum.

pub mod contract;
pub mod decode;
pub mod transaction;
pub mod validation;

pub use contract::ContractError;
pub use transaction::TransactionError;
pub use validation::ValidationError;

use thiserror::Error;

/// Central error type for the PerpCity SDK.
///
/// Composed from per-module error types. Use `#[from]` conversions to
/// return module-specific errors with `?`:
///
/// ```rust,ignore
/// // Inside client/transactions.rs:
/// Err(TransactionError::GasUnavailable { reason: "..." }.into())
/// // Automatically converts to PerpCityError::Transaction(...)
/// ```
///
/// Callers can pattern-match on the variant to determine which layer
/// failed and decide how to handle it.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum PerpCityError {
    /// Transaction lifecycle error (simulation, signing, gas, pipeline).
    #[error(transparent)]
    Transaction(#[from] TransactionError),

    /// Input validation error (prices, margins, ticks, leverage).
    #[error(transparent)]
    Validation(#[from] ValidationError),

    /// On-chain protocol state error (perps, positions, events, quotes).
    #[error(transparent)]
    Contract(#[from] ContractError),

    /// Alloy RPC / transport error.
    #[error(transparent)]
    Rpc(#[from] alloy::transports::TransportError),

    /// Alloy contract ABI error.
    #[error(transparent)]
    Abi(#[from] alloy::contract::Error),

    /// JSON serialization / deserialization error.
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
}

impl PerpCityError {
    /// Returns `true` if the error indicates a pre-flight simulation
    /// detected a contract revert (no gas was burned).
    pub fn is_simulation_revert(&self) -> bool {
        matches!(
            self,
            Self::Transaction(TransactionError::SimulationReverted { .. })
        )
    }

    /// Returns `true` if the error is likely transient and worth retrying
    /// (RPC errors, gas unavailable, etc.).
    ///
    /// A pre-flight simulation that the node answered — a decoded
    /// `SimulationReverted`, or a `SimulationFailed` (empty revert, out of
    /// gas inside the pinned limit) — is deterministic and never transient;
    /// `GasUnavailable` covers the simulation that got no answer at all.
    ///
    /// `NonceDesynced` is transient by construction: it clears itself once
    /// in-flight transactions drain and the next send resyncs from chain,
    /// so callers should back off briefly rather than give up.
    ///
    /// `BlockUnavailable` (a lagging replica briefly missing the pinned
    /// header) and `StorageReadFailed` with a transport `source` are
    /// stale-replica / network conditions — retryable. A
    /// `StorageReadFailed` without a source means the response had an
    /// unexpected shape, which retrying will not fix.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Rpc(_)
                | Self::Transaction(TransactionError::GasUnavailable { .. })
                | Self::Transaction(TransactionError::ReceiptTimeout { .. })
                | Self::Transaction(TransactionError::NonceDesynced { .. })
                | Self::Contract(ContractError::BlockUnavailable { .. })
                | Self::Contract(ContractError::StorageReadFailed {
                    source: Some(_),
                    ..
                })
        )
    }
}

/// Convenience alias used throughout the SDK.
pub type Result<T> = std::result::Result<T, PerpCityError>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Consumers key retry behaviour off this classification (backoff loops
    /// treat transients as "retry politely"), so it is API surface, not an
    /// implementation detail.
    #[test]
    fn desync_is_transient_and_reverts_are_not() {
        let desynced: PerpCityError = TransactionError::NonceDesynced { in_flight: 2 }.into();
        assert!(
            desynced.is_transient(),
            "desync clears itself after drain + resync; callers must retry, not give up"
        );

        let revert: PerpCityError = TransactionError::SimulationReverted {
            error_name: "PriceImpactTooHigh".into(),
            selector: [0xfb, 0x30, 0xd0, 0x3a].into(),
            revert_data: None,
        }
        .into();
        assert!(!revert.is_transient(), "a contract revert is deterministic");

        let failed: PerpCityError = TransactionError::SimulationFailed {
            reason: "eth_call failed: execution reverted".into(),
        }
        .into();
        assert!(
            !failed.is_transient(),
            "an empty revert is the node's answer, not a network condition"
        );

        let out_of_gas: PerpCityError = TransactionError::OutOfGas {
            tx_hash: [0x11; 32].into(),
            gas_used: 372_314,
            gas_limit: 377_451,
        }
        .into();
        assert!(
            !out_of_gas.is_transient(),
            "the send evicts the estimate, so a retry is the caller's call — not a backoff loop's"
        );
        assert!(revert.is_simulation_revert());
    }

    /// Typed revert matching compares raw selectors, not strings, so it
    /// keeps working whatever the decoded name looks like.
    #[test]
    fn is_revert_matches_by_selector() {
        use alloy::sol_types::SolError;

        use crate::contracts::Perp;

        let revert = TransactionError::SimulationReverted {
            error_name: "NotLiquidatable".into(),
            selector: Perp::NotLiquidatable::SELECTOR.into(),
            revert_data: None,
        };
        assert!(revert.is_revert::<Perp::NotLiquidatable>());
        assert!(!revert.is_revert::<Perp::NonMakerPosition>());

        let other = TransactionError::GasUnavailable {
            reason: "down".into(),
        };
        assert!(!other.is_revert::<Perp::NotLiquidatable>());
    }

    /// The read-path errors documented as retryable must classify as
    /// transient, and a malformed-response storage failure (no transport
    /// source) must not.
    #[test]
    fn stale_replica_read_failures_are_transient() {
        let unavailable: PerpCityError = ContractError::BlockUnavailable { number: 1 }.into();
        assert!(
            unavailable.is_transient(),
            "a lagging replica missing the pinned header clears on retry"
        );

        let transport: PerpCityError = ContractError::StorageReadFailed {
            context: "tick 60 funding".into(),
            source: Some(std::sync::Arc::new(
                alloy::transports::TransportErrorKind::custom_str("replica dropped the read"),
            )),
        }
        .into();
        assert!(transport.is_transient(), "transport-caused reads retry");

        let malformed: PerpCityError = ContractError::StorageReadFailed {
            context: "extsload word count".into(),
            source: None,
        }
        .into();
        assert!(
            !malformed.is_transient(),
            "an unexpected response shape does not fix itself"
        );
    }
}
