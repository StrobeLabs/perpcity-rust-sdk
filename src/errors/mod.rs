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

#![doc = "\n\nThe design of this module: [`src/errors/DESIGN.md`](https://github.com/StrobeLabs/perpcity-rust-sdk/blob/main/src/errors/DESIGN.md)."]

pub mod contract;
pub mod decode;
pub mod transaction;
pub mod validation;

pub use contract::ContractError;
pub use transaction::TransactionError;
pub use validation::ValidationError;

use alloy::primitives::FixedBytes;
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
    /// `BroadcastFailed` and `ReceiptTimeout` are transient: the
    /// transaction may still land, and its hash is on the error.
    ///
    /// `NonceDesynced` and `TooManyInFlight` are transient by construction:
    /// both clear themselves as in-flight transactions drain, so callers
    /// should back off briefly rather than give up. A full pipeline in
    /// particular is a caller sending faster than the chain confirms, which
    /// the next receipt fixes.
    ///
    /// `BlockUnavailable` (a lagging replica briefly missing the pinned
    /// header) and `StorageReadFailed` with a transport `source` are
    /// stale-replica / network conditions — retryable. A
    /// `StorageReadFailed` without a source means the response had an
    /// unexpected shape, which retrying will not fix.
    ///
    /// `LogsRejected` is a server's refusal of an `eth_getLogs` request at
    /// its narrowest, and `StateUnavailable` a full node's refusal of
    /// pruned state, so neither is transient.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Rpc(_)
                | Self::Transaction(TransactionError::GasUnavailable { .. })
                | Self::Transaction(TransactionError::BroadcastFailed { .. })
                | Self::Transaction(TransactionError::ReceiptTimeout { .. })
                | Self::Transaction(TransactionError::NonceDesynced { .. })
                | Self::Transaction(TransactionError::TooManyInFlight { .. })
                | Self::Transaction(TransactionError::TakerNotClosed { .. })
                | Self::Contract(ContractError::BlockUnavailable { .. })
                | Self::Contract(ContractError::StorageReadFailed {
                    source: Some(_),
                    ..
                })
        )
    }

    /// Hash of the signed transaction when the failure came at or after
    /// the broadcast; see [`TransactionError::tx_hash`].
    ///
    /// For an error from [`TxBuilder::send`](crate::TxBuilder::send),
    /// `None` means nothing was broadcast, whatever the variant: a
    /// `Some` hash may have landed, so look up its receipt before treating
    /// the send's effect as absent.
    pub fn tx_hash(&self) -> Option<FixedBytes<32>> {
        match self {
            Self::Transaction(e) => e.tx_hash(),
            _ => None,
        }
    }
}

/// Convenience alias used throughout the SDK.
pub type Result<T> = std::result::Result<T, PerpCityError>;

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;
    use alloy::transports::TransportErrorKind;

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

        // A full pipeline clears itself the same way, and classifying it the
        // other way round made a backoff loop give up on the condition that
        // resolves itself and retry the one that does not.
        let full: PerpCityError = TransactionError::TooManyInFlight { count: 8, max: 8 }.into();
        assert!(
            full.is_transient(),
            "a full pipeline drains as receipts arrive; nothing was signed or sent"
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

        let not_closed: PerpCityError = TransactionError::TakerNotClosed {
            tx_hash: [0x22; 32].into(),
            pos_id: U256::from(1837u64),
        }
        .into();
        assert!(
            not_closed.is_transient(),
            "the close reads the delta again on retry, so the next attempt can close"
        );
        assert_eq!(not_closed.tx_hash(), Some([0x22; 32].into()));

        let range: PerpCityError = ValidationError::InvalidBlockRange {
            from_block: 2,
            to_block: 1,
        }
        .into();
        assert!(!range.is_transient(), "a reversed range stays reversed");

        let refused: PerpCityError = ContractError::LogsRejected {
            from_block: 7,
            to_block: 7,
            source: alloy::transports::TransportErrorKind::http_error(403, String::new()),
        }
        .into();
        assert!(
            !refused.is_transient(),
            "the server refused this request at its narrowest; a retry gets the same answer"
        );

        let mined_revert: PerpCityError = TransactionError::Reverted {
            tx_hash: [0x44; 32].into(),
            reason: "transaction 0x44… reverted".into(),
        }
        .into();
        assert!(
            !mined_revert.is_transient(),
            "a mined revert is final; its nonce is consumed"
        );

        let receipt_timeout: PerpCityError = TransactionError::ReceiptTimeout {
            tx_hash: [0x22; 32].into(),
            reason: "no receipt after 30s".into(),
        }
        .into();
        assert!(
            receipt_timeout.is_transient(),
            "the transaction may still mine; the caller reconciles by hash"
        );

        let broadcast_failed: PerpCityError = TransactionError::BroadcastFailed {
            tx_hash: [0x33; 32].into(),
            source: TransportErrorKind::custom_str("connection reset"),
        }
        .into();
        assert!(
            broadcast_failed.is_transient(),
            "a failed broadcast was a transient transport error before it was typed"
        );

        let no_capacity: PerpCityError = ValidationError::NoBandCapacity {
            lower: 20_000,
            upper: 30_000,
        }
        .into();
        assert!(
            !no_capacity.is_transient(),
            "a band's shape against the price does not change on retry"
        );
    }

    /// Callers reconcile an unknown outcome by receipt, so every error after
    /// the broadcast must expose its hash, and no pre-broadcast error may.
    #[test]
    fn tx_hash_is_set_exactly_from_the_broadcast_onward() {
        let hash = FixedBytes::<32>::repeat_byte(0x55);
        let sent = [
            TransactionError::BroadcastFailed {
                tx_hash: hash,
                source: TransportErrorKind::custom_str("connection reset"),
            },
            TransactionError::ReceiptTimeout {
                tx_hash: hash,
                reason: "no receipt after 30s".into(),
            },
            TransactionError::Reverted {
                tx_hash: hash,
                reason: "reverted".into(),
            },
            TransactionError::OutOfGas {
                tx_hash: hash,
                gas_used: 100,
                gas_limit: 100,
            },
        ];
        for err in &sent {
            assert_eq!(err.tx_hash(), Some(hash), "{err}");
        }

        let unsent = [
            TransactionError::SimulationFailed {
                reason: "execution reverted".into(),
            },
            TransactionError::GasUnavailable {
                reason: "down".into(),
            },
            TransactionError::SigningFailed {
                reason: "kms".into(),
            },
            TransactionError::NonceDesynced { in_flight: 1 },
            TransactionError::TooManyInFlight { count: 4, max: 4 },
        ];
        for err in &unsent {
            assert_eq!(err.tx_hash(), None, "{err}");
        }

        let wrapped: PerpCityError = sent.into_iter().next().unwrap().into();
        assert_eq!(wrapped.tx_hash(), Some(hash));
        let pre_broadcast_rpc: PerpCityError =
            TransportErrorKind::custom_str("nonce count read failed").into();
        assert_eq!(
            pre_broadcast_rpc.tx_hash(),
            None,
            "a send's Rpc error comes before the broadcast"
        );
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
            source: Some(std::sync::Arc::new(TransportErrorKind::custom_str(
                "replica dropped the read",
            ))),
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
